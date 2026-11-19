use crate::media::Media;
use crate::placement::CapsuleData;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

#[derive(Debug)]
pub struct Cover {
    pub size: u32,
    pub pixels: Vec<u8>,
}

#[derive(Clone)]
pub struct UiSnapshot {
    pub media: Option<Media>,
    pub cover: Option<Arc<Cover>>,
    pub status: &'static str,
    pub placement: CapsuleData,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            media: None,
            cover: None,
            status: "等待 Wallpaper Engine",
            placement: CapsuleData::default(),
        }
    }
}

pub fn data_dir(local: bool) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RAIN_NIGHT_DATA_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Some(dir);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("portable-data")))
        .filter(|p| p.is_dir())
    {
        return Some(dir);
    }
    std::env::var_os(if local { "LOCALAPPDATA" } else { "APPDATA" })
        .map(|p| PathBuf::from(p).join("RainNightLyrics"))
}

pub fn log(status: &'static str) {
    let Some(dir) = data_dir(true) else { return };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let file = dir.join("lyrics.log");
    if fs::metadata(&file).is_ok_and(|m| m.len() + status.len() as u64 + 2 > 256 * 1024) {
        let old = dir.join("lyrics.previous.log");
        let _ = fs::remove_file(&old);
        if fs::rename(&file, old).is_err() {
            return;
        }
    }
    if let Ok(mut out) = OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(out, "{status}");
    }
}

#[derive(Default)]
pub struct UiBridge {
    snapshot: Mutex<UiSnapshot>,
    hwnd: AtomicIsize,
    size: AtomicU32,
    pub finished: AtomicBool,
}

impl UiBridge {
    pub fn attach(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::Release);
    }
    pub fn cover_size(&self, size: u32) {
        self.size.store(size.clamp(16, 256), Ordering::Relaxed);
    }
    pub fn desired_size(&self) -> u32 {
        let n = self.size.load(Ordering::Relaxed);
        if n == 0 { 34 } else { n }
    }
    pub fn latest(&self) -> UiSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
    pub fn publish(&self, next: UiSnapshot) {
        let mut last = self.snapshot.lock().unwrap();
        let same_cover = match (&last.cover, &next.cover) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let same_media = match (&last.media, &next.media) {
            (Some(a), Some(b)) => {
                a.song == b.song
                    && a.playing == b.playing
                    && a.rate == b.rate
                    && a.duration == b.duration
                    && if a.playing {
                        !b.needs_sync(a)
                    } else {
                        a.position == b.position
                    }
            }
            (None, None) => true,
            _ => false,
        };
        if last.status == next.status
            && same_cover
            && same_media
            && last.placement == next.placement
        {
            return;
        }
        if last.status != next.status {
            log(next.status)
        }
        *last = next;
        drop(last);
        let hwnd = HWND(self.hwnd.load(Ordering::Acquire) as *mut _);
        if !hwnd.is_invalid() {
            unsafe {
                let _ = PostMessageW(hwnd, WM_APP + 1, WPARAM(0), LPARAM(0));
            }
        }
    }
}
