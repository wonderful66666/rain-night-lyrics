use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::os::windows::{ffi::OsStringExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateMutexW, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, ReleaseMutex,
    WaitForSingleObject,
};
use windows::core::{Owned, PWSTR, w};

pub const WORKSHOP_ID: &str = "3842256065";

pub struct StopSignal(Owned<HANDLE>);
impl StopSignal {
    pub fn create() -> windows::core::Result<Self> {
        unsafe {
            Ok(Self(Owned::new(
                windows::Win32::System::Threading::CreateEventW(
                    None,
                    true,
                    false,
                    w!("Local\\RainNightLyrics-Subscriber-Stop"),
                )?,
            )))
        }
    }
    pub fn requested(&self) -> bool {
        unsafe { WaitForSingleObject(*self.0, 0) == WAIT_OBJECT_0 }
    }
    pub fn send() -> bool {
        use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
        unsafe {
            let Ok(event) = OpenEventW(
                EVENT_MODIFY_STATE,
                false,
                w!("Local\\RainNightLyrics-Subscriber-Stop"),
            ) else {
                return false;
            };
            let event = Owned::new(event);
            SetEvent(*event).is_ok()
        }
    }
}

pub struct SingleInstance(Owned<HANDLE>);
impl SingleInstance {
    pub fn acquire() -> windows::core::Result<Option<Self>> {
        unsafe {
            let lock = Owned::new(CreateMutexW(
                None,
                false,
                w!("Local\\RainNight_MusicCapsule-Lyrics"),
            )?);
            Ok(match WaitForSingleObject(*lock, 0) {
                WAIT_OBJECT_0 | WAIT_ABANDONED => Some(Self(lock)),
                _ => None,
            })
        }
    }
}
impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(*self.0);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Engine {
    exe: PathBuf,
    pid: u32,
}

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let process = Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
        let mut buf = vec![0u16; 32_768];
        let mut size = buf.len() as u32;
        QueryFullProcessImageNameW(
            *process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .ok()?;
        Some(OsString::from_wide(&buf[..size as usize]).into())
    }
}

fn session_id(pid: u32) -> Option<u32> {
    let mut session = 0;
    unsafe {
        ProcessIdToSessionId(pid, &mut session).ok()?;
    }
    Some(session)
}

fn is_ours(file: &str) -> bool {
    let path = Path::new(file);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let parts: Vec<_> = path
        .components()
        .rev()
        .take(6)
        .map(|p| p.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    parts.len() == 6
        && matches!(
            parts[0].as_str(),
            "scene.pkg" | "project.json" | "scene.json"
        )
        && parts[1..] == [WORKSHOP_ID, "431960", "content", "workshop", "steamapps"]
}

fn user_settings<'a>(config: &'a Value, user: &str) -> Option<&'a Value> {
    config
        .as_object()
        .and_then(|root| root.iter().find(|(k, _)| k.eq_ignore_ascii_case(user)))
        .and_then(|(_, entry)| entry.get("general"))
}

fn selected(config: &Value, user: &str) -> Vec<String> {
    let Some(monitors) = user_settings(config, user)
        .and_then(|entry| entry.pointer("/wallpaperconfig/selectedwallpapers"))
        .and_then(Value::as_object)
    else {
        return vec![];
    };
    monitors
        .iter()
        .filter(|(name, entry)| {
            !name.is_empty()
                && name.len() <= 255
                && !name.starts_with('-')
                && !name.chars().any(|ch| ch.is_control() || ch == '"')
                && entry
                    .get("file")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn reload_target<'a>(
    config: &'a Value,
    user: &str,
    display: &str,
) -> Option<(&'a str, Option<&'a str>)> {
    let settings = user_settings(config, user)?;
    if let Some(device) = settings.pointer("/user/audioinputdevice") {
        let device = device.as_str()?;
        if !device.is_empty() && !device.eq_ignore_ascii_case("default") {
            return None;
        }
    }
    let entry = settings
        .pointer("/wallpaperconfig/selectedwallpapers")?
        .get(display)?;
    if entry
        .get("playlist")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        return None;
    }
    let file = entry.get("file")?.as_str()?;
    let preset = entry
        .get("preset")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    is_ours(file).then_some((file, preset))
}

impl Engine {
    pub fn find() -> Option<Self> {
        let session = session_id(std::process::id())?;
        unsafe {
            let snapshot = Owned::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?);
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            Process32FirstW(*snapshot, &mut entry).ok()?;
            let mut other = None;
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if (name.eq_ignore_ascii_case("wallpaper64.exe")
                    || name.eq_ignore_ascii_case("wallpaper32.exe"))
                    && session_id(entry.th32ProcessID) == Some(session)
                    && let Some(exe) = process_path(entry.th32ProcessID)
                    && exe.with_file_name("config.json").is_file()
                {
                    let steam = exe.parent().is_some_and(|p| {
                        p.to_string_lossy()
                            .to_ascii_lowercase()
                            .ends_with(r"\steamapps\common\wallpaper_engine")
                    });
                    let engine = Self {
                        exe,
                        pid: entry.th32ProcessID,
                    };
                    if steam {
                        return Some(engine);
                    }
                    if other.is_none() || !engine.displays().is_empty() {
                        other = Some(engine);
                    }
                }
                if Process32NextW(*snapshot, &mut entry).is_err() {
                    break;
                }
            }
            other
        }
    }

    fn config(&self) -> Option<Value> {
        let bytes = fs::read(self.exe.with_file_name("config.json")).ok()?;
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        serde_json::from_slice(bytes).ok()
    }

    pub fn displays(&self) -> Vec<String> {
        self.config()
            .map(|c| selected(&c, &std::env::var("USERNAME").unwrap_or_default()))
            .unwrap_or_default()
    }

    pub fn reload_audio(&self, displays: &[String]) -> (usize, bool) {
        let user = std::env::var("USERNAME").unwrap_or_default();
        let mut requested = 0;
        let mut retry = false;
        for display in displays {
            if process_path(self.pid).as_ref() != Some(&self.exe) {
                break;
            }
            let Some(config) = self.config() else {
                retry = true;
                continue;
            };
            let Some((file, preset)) = reload_target(&config, &user, display) else {
                continue;
            };
            if !Path::new(file).is_file() {
                continue;
            }
            let mut command = Command::new(&self.exe);
            command.args([
                "-control",
                "openWallpaper",
                "-location",
                display,
                "-file",
                file,
            ]);
            if let Some(preset) = preset {
                command.args(["-preset", preset]);
            }
            if self.control(&mut command) {
                requested += 1;
            } else {
                retry = true;
            }
        }
        (requested, retry)
    }

    pub fn send(&self, display: &str, value: &str) -> bool {
        if process_path(self.pid).as_ref() != Some(&self.exe)
            || !self.displays().iter().any(|d| d == display)
        {
            return false;
        }
        let raw = format!("RAW~({{\"capsuleautomaticallyrics\":\"{value}\"}})~END");
        self.control(
            Command::new(&self.exe)
                .args([
                    "-control",
                    "applyProperties",
                    "-location",
                    display,
                    "-properties",
                ])
                .raw_arg(raw),
        )
    }

    fn control(&self, command: &mut Command) -> bool {
        let child = command
            .current_dir(self.exe.parent().unwrap())
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return false;
        };
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                _ => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn targets_only_this_subscription_across_libraries_and_monitors() {
        let file =
            format!("Q:/Steam Library/steamapps/workshop/content/431960/{WORKSHOP_ID}/scene.pkg");
        let config = json!({"Tester":{"general":{"wallpaperconfig":{"selectedwallpapers":{
            "Monitor0":{"file":file.replace(WORKSHOP_ID,"12345")}, "Monitor1":{"file":file}
        }}}}});
        assert_eq!(selected(&config, "tester"), ["Monitor1"]);
        assert!(selected(&config, "someone else").is_empty());
        assert!(!is_ours(&file.replace("431960", "431961")));
        assert!(!is_ours(&file.replace("scene.pkg", "preview.gif")));
        assert!(!is_ours("scene.pkg"));
    }

    #[test]
    fn audio_recovery_respects_the_selected_screen_input_and_playlist() {
        let file =
            format!("Q:/Steam Library/steamapps/workshop/content/431960/{WORKSHOP_ID}/scene.pkg");
        let mut config = json!({"Listener":{"general":{"wallpaperconfig":{"selectedwallpapers":{
            "Monitor0":{"file":file,"preset":"Evening"}, "Monitor1":{"file":file.replace(WORKSHOP_ID, "10")}
        }}}}});
        assert_eq!(
            reload_target(&config, "listener", "Monitor0"),
            Some((file.as_str(), Some("Evening")))
        );
        assert!(reload_target(&config, "listener", "Monitor1").is_none());
        config["Listener"]["general"]["user"] = json!({"audioinputdevice":"microphone"});
        assert!(reload_target(&config, "listener", "Monitor0").is_none());
        config["Listener"]["general"]["user"]["audioinputdevice"] = json!("Default");
        config["Listener"]["general"]["wallpaperconfig"]["selectedwallpapers"]["Monitor0"]["playlist"] =
            json!("evening rotation");
        assert!(reload_target(&config, "listener", "Monitor0").is_none());
    }
}
