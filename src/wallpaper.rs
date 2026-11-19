use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
    process::CommandExt,
};
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::Security::Cryptography::{
    CERT_NAME_ATTR_TYPE, CertGetNameStringW, szOID_ORGANIZATION_NAME,
};
use windows::Win32::Security::WinTrust::*;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ, GetDriveTypeW,
    GetFileInformationByHandle,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::*;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateMutexW, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, ReleaseMutex,
    WaitForSingleObject,
};
use windows::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};
use windows::core::{Owned, PCWSTR, PWSTR, w};

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
    pub fn handle(&self) -> HANDLE {
        *self.0
    }
    pub fn show_event() -> windows::core::Result<Self> {
        unsafe {
            Ok(Self(Owned::new(
                windows::Win32::System::Threading::CreateEventW(
                    None,
                    false,
                    false,
                    w!("Local\\RainNightLyrics-Subscriber-Show"),
                )?,
            )))
        }
    }
    pub fn send() -> bool {
        Self::signal(w!("Local\\RainNightLyrics-Subscriber-Stop"))
    }
    pub fn show() -> bool {
        Self::signal(w!("Local\\RainNightLyrics-Subscriber-Show"))
    }
    fn signal(name: windows::core::PCWSTR) -> bool {
        use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
        unsafe {
            let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, name) else {
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

#[derive(Clone, Debug)]
pub struct Engine {
    exe: PathBuf,
    pid: u32,
    signed_file: Arc<fs::File>,
}

impl PartialEq for Engine {
    fn eq(&self, other: &Self) -> bool {
        self.pid == other.pid && self.exe == other.exe
    }
}
impl Eq for Engine {}

fn disk_letter(path: &Path) -> Option<u8> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(letter) => Some(letter),
            _ => None,
        },
        _ => None,
    }
}

fn local_unlinked(path: &Path) -> bool {
    let Some(letter) = disk_letter(path) else {
        return false;
    };
    let root = [u16::from(letter), b':' as u16, b'\\' as u16, 0];
    if !matches!(
        unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) },
        DRIVE_FIXED | DRIVE_REMOVABLE
    ) {
        return false;
    }
    let ancestors: Vec<_> = path.ancestors().collect();
    ancestors.iter().rev().all(|p| {
        fs::symlink_metadata(p)
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0)
    })
}

fn registered_dir(key: PCWSTR, value: PCWSTR, view: REG_ROUTINE_FLAGS) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    let mut bytes = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value,
            RRF_RT_REG_SZ | view,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
        .ok()
        .ok()?;
    }
    let length = buf.iter().position(|&c| c == 0)?;
    Some(OsString::from_wide(&buf[..length]).into())
}

fn engine_in_dir(exe: &Path, dir: &Path) -> bool {
    disk_letter(dir).is_some()
        && exe.parent().is_some_and(|p| {
            p.components().count() == dir.components().count()
                && p.components()
                    .zip(dir.components())
                    .all(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str()))
        })
        && exe.file_name().is_some_and(|name| {
            name.eq_ignore_ascii_case("wallpaper64.exe")
                || name.eq_ignore_ascii_case("wallpaper32.exe")
        })
}

fn is_installed_engine(exe: &Path) -> bool {
    [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY]
        .into_iter()
        .any(|view| {
            let app = registered_dir(
                w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Steam App 431960"),
                w!("InstallLocation"),
                view,
            );
            let steam = || {
                registered_dir(w!("SOFTWARE\\Valve\\Steam"), w!("InstallPath"), view)
                    .map(|p| p.join("steamapps/common/wallpaper_engine"))
            };
            app.is_some_and(|dir| engine_in_dir(exe, &dir))
                || steam().is_some_and(|dir| engine_in_dir(exe, &dir))
        })
}

fn wallpaper_signature(path: &Path, file: &fs::File) -> bool {
    let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(name.as_ptr()),
        hFile: HANDLE(file.as_raw_handle()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_DISABLE_MD2_MD4,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    unsafe {
        let result = WinVerifyTrust(HWND::default(), &mut action, (&raw mut data).cast());
        let mut organization = [0u16; 128];
        let known_publisher = result == 0
            && (|| {
                let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
                if provider.is_null() {
                    return false;
                }
                let signer = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
                if signer.is_null() {
                    return false;
                }
                let cert = WTHelperGetProvCertFromChain(signer, 0);
                if cert.is_null() || (*cert).pCert.is_null() {
                    return false;
                }
                let n = CertGetNameStringW(
                    (*cert).pCert,
                    CERT_NAME_ATTR_TYPE,
                    0,
                    Some(szOID_ORGANIZATION_NAME.0.cast()),
                    Some(&mut organization),
                ) as usize;
                n > 1
                    && n <= organization.len()
                    && organization[..n - 1]
                        .iter()
                        .copied()
                        .eq("Skutta Software GmbH".encode_utf16())
            })();
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(HWND::default(), &mut action, (&raw mut data).cast());
        known_publisher
    }
}

fn signed_engine_file(pid: u32, exe: &Path) -> Option<Arc<fs::File>> {
    type LastFile = (u32, PathBuf, Weak<fs::File>);
    static CACHE: Mutex<Option<LastFile>> = Mutex::new(None);
    let mut cache = CACHE.lock().ok()?;
    if let Some((old_pid, old_exe, old_file)) = cache.as_ref()
        && *old_pid == pid
        && old_exe == exe
        && let Some(file) = old_file.upgrade()
    {
        return Some(file);
    }
    if !is_installed_engine(exe) || !local_unlinked(exe) {
        return None;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(exe)
        .ok()?;
    if !wallpaper_signature(exe, &file) {
        return None;
    }
    let file = Arc::new(file);
    *cache = Some((pid, exe.into(), Arc::downgrade(&file)));
    Some(file)
}

fn file_id(file: &fs::File) -> Option<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.ok()?;
    (info.nFileIndexHigh != 0 || info.nFileIndexLow != 0).then_some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
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
    if disk_letter(path).is_none() {
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReloadReason {
    AudioOutput,
    LyricsCleared,
}

fn in_playlist(entry: &Value) -> bool {
    entry
        .get("playlist")
        .is_some_and(|v| !v.is_null() && v.as_str() != Some("") && v.as_bool() != Some(false))
}

fn reload_target<'a>(
    config: &'a Value,
    user: &str,
    display: &str,
    reason: ReloadReason,
) -> Option<(&'a str, Option<&'a str>)> {
    let settings = user_settings(config, user)?;
    if reason == ReloadReason::AudioOutput
        && let Some(device) = settings.pointer("/user/audioinputdevice")
    {
        let device = device.as_str()?;
        if !device.is_empty() && !device.eq_ignore_ascii_case("default") {
            return None;
        }
    }
    let entry = settings
        .pointer("/wallpaperconfig/selectedwallpapers")?
        .get(display)?;
    if in_playlist(entry) {
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
                    && let Some(file) = signed_engine_file(entry.th32ProcessID, &exe)
                    && local_unlinked(&exe.with_file_name("config.json"))
                {
                    return Some(Self {
                        exe,
                        pid: entry.th32ProcessID,
                        signed_file: file,
                    });
                }
                if Process32NextW(*snapshot, &mut entry).is_err() {
                    break;
                }
            }
            None
        }
    }

    fn config(&self) -> Option<Value> {
        const MAX_BYTES: u64 = 32 * 1024 * 1024;
        let path = self.exe.with_file_name("config.json");
        if !local_unlinked(&path) {
            return None;
        }
        let file = fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > MAX_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        serde_json::from_slice(bytes).ok()
    }

    pub fn displays(&self) -> Vec<String> {
        self.config()
            .map(|c| selected(&c, &std::env::var("USERNAME").unwrap_or_default()))
            .unwrap_or_default()
    }
    pub fn scene(&self) -> (Vec<String>, crate::placement::CapsuleData, i64) {
        let mut data = crate::placement::CapsuleData::default();
        let Some(cfg) = self.config() else {
            return (Vec::new(), data, 0);
        };
        let name = std::env::var("USERNAME").unwrap_or_default();
        let screens = selected(&cfg, &name);
        let Some(display) = screens.first() else {
            return (screens, data, 0);
        };
        data.display = Some(display.clone());
        data.fonts = self.exe.parent().map(|dir| dir.join("assets/fonts"));
        if let Some(settings) = user_settings(&cfg, &name) {
            data.paths = crate::placement::display_paths(settings, display);
        }
        let Some(user) = cfg
            .as_object()
            .and_then(|users| {
                users
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(&name))
            })
            .map(|(_, v)| v)
        else {
            return (screens, data, 0);
        };
        let selected_file = user
            .pointer("/general/wallpaperconfig/selectedwallpapers")
            .and_then(|m| m.get(display))
            .and_then(|e| e.get("file"))
            .and_then(Value::as_str);
        let Some(file) = selected_file else {
            return (screens, data, 0);
        };
        let mut project = PathBuf::from(file);
        project.set_file_name("project.json");
        let props = user
            .get("wproperties")
            .and_then(Value::as_object)
            .and_then(|map| {
                map.iter().find(|(path, _)| {
                    Path::new(path) == Path::new(file) || Path::new(path) == project
                })
            })
            .and_then(|(_, p)| p.get(display));
        let number = |key: &str| {
            props
                .and_then(|p| p.get(key))
                .map(|v| v.get("value").unwrap_or(v))
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .unwrap_or(0)
        };
        data.font = number("capsulefont").clamp(0, 2) as u8;
        (
            screens,
            data,
            number("capsulelyricdelay").clamp(-30000, 30000),
        )
    }

    pub fn in_playlist(&self, display: &str) -> bool {
        let user = std::env::var("USERNAME").unwrap_or_default();
        self.config().is_some_and(|cfg| {
            user_settings(&cfg, &user)
                .and_then(|s| s.pointer("/wallpaperconfig/selectedwallpapers"))
                .and_then(|v| v.get(display))
                .is_some_and(in_playlist)
        })
    }
    pub fn reload(&self, displays: &[String], reason: ReloadReason) -> (usize, bool) {
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
            let Some((file, preset)) = reload_target(&config, &user, display, reason) else {
                continue;
            };
            if !local_unlinked(Path::new(file)) || !Path::new(file).is_file() {
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
        if !local_unlinked(&self.exe) || process_path(self.pid).as_ref() != Some(&self.exe) {
            return false;
        }
        let Ok(current_file) = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&self.exe)
        else {
            return false;
        };
        let verified = file_id(&self.signed_file);
        if verified.is_none() || verified != file_id(&current_file) {
            return false;
        }
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
    fn engine_identity_requires_exact_registered_directory_and_local_drive() {
        let installed = Path::new(r"Q:\My Library\steamapps\common\wallpaper_engine");
        assert!(engine_in_dir(
            Path::new(r"q:\my library\SteamApps\common\wallpaper_engine\Wallpaper64.exe"),
            installed
        ));
        assert!(!engine_in_dir(
            Path::new(r"P:\My Library\steamapps\common\wallpaper_engine\wallpaper64.exe"),
            installed
        ));
        assert!(!engine_in_dir(
            &installed.join("other/wallpaper64.exe"),
            installed
        ));
        assert!(!engine_in_dir(&installed.join("other.exe"), installed));
        assert!(disk_letter(Path::new(r"\\server\share\Steam")).is_none());
        assert!(disk_letter(Path::new(r"\\?\Q:\Steam")).is_none());
        assert!(disk_letter(Path::new(r"Q:\temp\..\Steam")).is_none());
        assert!(!is_ours(&format!(
            r"\\server\share\SteamApps\workshop\content\431960\{WORKSHOP_ID}\scene.pkg"
        )));
    }

    #[test]
    fn unrelated_binary_is_not_the_wallpaper_publisher() {
        let exe = std::env::current_exe().unwrap();
        let file = fs::File::open(&exe).unwrap();
        assert!(!wallpaper_signature(&exe, &file));
    }

    #[test]
    #[ignore = "requires a local Steam installation of Wallpaper Engine"]
    fn verify_installed_wallpaper_publisher() {
        let path = [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY]
            .into_iter()
            .find_map(|view| {
                registered_dir(
                    w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Steam App 431960"),
                    w!("InstallLocation"),
                    view,
                )
            })
            .unwrap()
            .join("wallpaper64.exe");
        assert!(signed_engine_file(0, &path).is_some());
    }
    #[test]
    fn first_matched_screen_follows_config_order() {
        let file = format!("Q:/Library/steamapps/workshop/content/431960/{WORKSHOP_ID}/scene.pkg");
        let cfg = json!({"User":{"general":{"wallpaperconfig":{"selectedwallpapers":{"Monitor2":{"file":file},"Monitor0":{"file":file}}}}}});
        let restored: Value = serde_json::from_slice(&serde_json::to_vec(&cfg).unwrap()).unwrap();
        assert_eq!(selected(&restored, "user"), ["Monitor2", "Monitor0"]);
    }
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
            reload_target(&config, "listener", "Monitor0", ReloadReason::AudioOutput),
            Some((file.as_str(), Some("Evening")))
        );
        assert!(
            reload_target(&config, "listener", "Monitor1", ReloadReason::LyricsCleared).is_none()
        );
        config["Listener"]["general"]["user"] = json!({"audioinputdevice":"microphone"});
        assert!(
            reload_target(&config, "listener", "Monitor0", ReloadReason::AudioOutput).is_none()
        );
        assert!(
            reload_target(&config, "listener", "Monitor0", ReloadReason::LyricsCleared).is_some()
        );
        config["Listener"]["general"]["user"]["audioinputdevice"] = json!("Default");
        config["Listener"]["general"]["wallpaperconfig"]["selectedwallpapers"]["Monitor0"]["playlist"] =
            json!("evening rotation");
        assert!(
            reload_target(&config, "listener", "Monitor0", ReloadReason::LyricsCleared).is_none()
        );
    }
}
