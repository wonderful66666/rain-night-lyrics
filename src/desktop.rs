use crate::media::Media;
use crate::placement::CapsuleData;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU32, AtomicU64, Ordering},
};
use std::thread::Thread;
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
    lookup_requested: AtomicBool,
    confirmation: Mutex<LookupConfirmation>,
    policy: LookupPolicy,
    worker: OnceLock<Thread>,
    hwnd: AtomicIsize,
    size: AtomicU32,
    pub finished: AtomicBool,
}

struct LookupPolicy {
    dir: Option<PathBuf>,
    revision: Arc<AtomicU64>,
    menu_source: AtomicU8,
    ask_pending: AtomicBool,
    prompt_failed: AtomicBool,
}

impl LookupPolicy {
    fn new(dir: Option<PathBuf>) -> Self {
        let revision = match dir.as_deref().and_then(crate::settings::auto_lookup) {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        Self {
            dir,
            revision: Arc::new(AtomicU64::new(revision)),
            menu_source: AtomicU8::new(0),
            ask_pending: AtomicBool::new(false),
            prompt_failed: AtomicBool::new(false),
        }
    }
}

impl Default for LookupPolicy {
    fn default() -> Self {
        Self::new(data_dir(false))
    }
}

#[derive(Default)]
struct LookupConfirmation {
    offer: Option<(u64, String)>,
    answer: Option<LookupAnswer>,
}

pub type LookupAnswer = (u64, Option<(String, String)>);

pub const LOOKUP_OFFER: u32 = WM_APP + 7;
pub const DISMISS_LOOKUP: u32 = WM_APP + 8;
pub const ASK_AUTO: u32 = WM_APP + 9;

impl UiBridge {
    pub fn bind_worker(&self) {
        let _ = self.worker.set(std::thread::current());
    }
    fn wake_worker(&self) {
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }
    fn post(&self, msg: u32) {
        let hwnd = HWND(self.hwnd.load(Ordering::Acquire) as *mut _);
        if !hwnd.is_invalid() {
            let _ = unsafe { PostMessageW(hwnd, msg, WPARAM(0), LPARAM(0)) };
        }
    }
    pub fn auto_policy(&self) -> (Option<bool>, u64) {
        let revision = self.policy.revision.load(Ordering::Acquire);
        let choice = match revision & 3 {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        (choice, revision)
    }
    pub fn auto_revision(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.policy.revision)
    }
    pub fn set_lookup_context(&self, manual_enabled: bool, ask_auto: bool, active_music: bool) {
        self.policy.menu_source.store(
            if active_music {
                2
            } else {
                u8::from(manual_enabled)
            },
            Ordering::Release,
        );
        let pending = ask_auto
            && self.auto_policy().0.is_none()
            && !self.policy.prompt_failed.load(Ordering::Acquire);
        if self.policy.ask_pending.swap(pending, Ordering::AcqRel) != pending {
            self.post(ASK_AUTO);
        }
    }
    pub fn can_request_lookup(&self) -> bool {
        match self.policy.menu_source.load(Ordering::Acquire) {
            2 => self.auto_policy().0 == Some(false),
            1 => true,
            _ => false,
        }
    }
    pub fn ask_auto_pending(&self) -> bool {
        self.policy.ask_pending.load(Ordering::Acquire) && self.auto_policy().0.is_none()
    }
    pub fn auto_prompt_failed(&self) {
        self.policy.prompt_failed.store(true, Ordering::Release);
        self.policy.ask_pending.store(false, Ordering::Release);
        log("无法打开自动查词询问窗口");
    }
    #[cfg(test)]
    pub fn with_settings_dir(dir: PathBuf) -> Self {
        Self {
            policy: LookupPolicy::new(Some(dir)),
            ..Default::default()
        }
    }
    pub fn choose_auto(&self, enabled: bool) -> std::io::Result<()> {
        let (old, revision) = self.auto_policy();
        let save = || {
            let dir = self
                .policy
                .dir
                .as_deref()
                .ok_or(std::io::ErrorKind::NotFound)?;
            crate::settings::set_auto_lookup(dir, enabled)
        };
        if enabled {
            save()?;
        }
        if old != Some(enabled) {
            let next = (revision & !3).wrapping_add(4) | if enabled { 2 } else { 1 };
            self.policy.revision.store(next, Ordering::Release);
        }
        self.policy.ask_pending.store(false, Ordering::Release);
        self.wake_worker();
        if !enabled {
            save()?;
        }
        Ok(())
    }
    pub fn request_lookup(&self) {
        self.lookup_requested.store(true, Ordering::Release);
        self.wake_worker();
    }
    pub fn take_lookup_request(&self) -> bool {
        self.lookup_requested.swap(false, Ordering::AcqRel)
    }
    pub fn offer_lookup_title(&self, offer: Option<(u64, String)>) {
        let mut state = self.confirmation.lock().unwrap();
        if state.offer == offer && (offer.is_some() || state.answer.is_none()) {
            return;
        }
        let message = if offer.is_some() {
            LOOKUP_OFFER
        } else {
            DISMISS_LOOKUP
        };
        state.answer = None;
        state.offer = offer;
        drop(state);
        self.post(message);
    }
    pub fn lookup_offer(&self) -> Option<(u64, String)> {
        self.confirmation.lock().unwrap().offer.clone()
    }
    pub fn lookup_offer_is_current(&self, id: u64) -> bool {
        self.confirmation
            .lock()
            .unwrap()
            .offer
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
    }
    pub fn answer_lookup(&self, id: u64, fields: Option<(String, String)>) {
        let mut state = self.confirmation.lock().unwrap();
        if state.offer.as_ref().is_some_and(|(n, _)| *n == id) {
            state.offer = None;
            state.answer = Some((id, fields));
            self.wake_worker();
        }
    }
    pub fn take_lookup_answer(&self) -> Option<LookupAnswer> {
        self.confirmation.lock().unwrap().answer.take()
    }
    pub fn attach(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::Release);
        if self.ask_auto_pending() {
            self.post(ASK_AUTO);
        }
        if self.lookup_offer().is_some() {
            self.post(LOOKUP_OFFER);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_consent_is_explicit_and_one_use() {
        let bridge = UiBridge::default();
        assert!(!bridge.take_lookup_request());
        assert_eq!(bridge.take_lookup_answer(), None);
        bridge.request_lookup();
        assert!(bridge.take_lookup_request());
        assert!(!bridge.take_lookup_request());
        bridge.offer_lookup_title(Some((1, "第一首".into())));
        bridge.answer_lookup(1, None);
        assert_eq!(bridge.take_lookup_answer(), Some((1, None)));
        bridge.offer_lookup_title(Some((2, "第二首".into())));
        bridge.offer_lookup_title(Some((3, "第三首".into())));
        let fields = Some(("手填歌名".to_string(), "本地歌手".to_string()));
        bridge.answer_lookup(2, fields.clone());
        assert_eq!(bridge.take_lookup_answer(), None);
        assert!(bridge.lookup_offer_is_current(3));
        bridge.answer_lookup(3, fields.clone());
        assert!(!bridge.lookup_offer_is_current(3));
        assert_eq!(bridge.take_lookup_answer(), Some((3, fields.clone())));
        assert_eq!(bridge.take_lookup_answer(), None);
        bridge.offer_lookup_title(Some((4, "第四首".into())));
        bridge.answer_lookup(4, fields);
        bridge.offer_lookup_title(None);
        assert_eq!(bridge.take_lookup_answer(), None);
    }
}
