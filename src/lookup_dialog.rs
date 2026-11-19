use crate::desktop::UiBridge;
use crate::media::valid_lookup_input;
use std::sync::Arc;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Result};

const TITLE: i32 = 101;
const ARTIST: i32 = 102;
const AUTO_ACCEPT: i32 = 103;
const AUTO_BODY: i32 = 104;
const ANSWER: u32 = WM_APP + 51;

fn template(automatic: bool) -> Vec<u32> {
    fn string(words: &mut Vec<u16>, value: &str) {
        words.extend(value.encode_utf16().chain([0]));
    }
    fn dword(words: &mut Vec<u16>, value: u32) {
        words.extend([value as u16, (value >> 16) as u16]);
    }
    let style =
        WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0 | (DS_CENTER | DS_MODALFRAME | DS_SETFONT) as u32;
    let mut words = Vec::new();
    dword(&mut words, style);
    dword(&mut words, 0);
    let (controls, width, height) = if automatic {
        (3, 308, 118)
    } else {
        (7, 282, 143)
    };
    words.extend([controls, 0, 0, width, height, 0, 0]);
    string(
        &mut words,
        if automatic {
            "要自动查歌词吗？"
        } else {
            "查找这一首的歌词"
        },
    );
    words.push(9);
    string(&mut words, "Microsoft YaHei UI");
    let edit = WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32;
    let manual_controls = [
        (
            0x82,
            0xffff,
            [12, 9, 258, 17],
            0,
            "确认后，仅将下方歌名发给网易云音乐或 LRCLIB。",
        ),
        (
            0x82,
            0xffff,
            [12, 29, 258, 11],
            0,
            "只发送这个歌名（必填，最多 120 字）",
        ),
        (0x81, TITLE as u16, [12, 42, 258, 16], edit, ""),
        (
            0x82,
            0xffff,
            [12, 67, 258, 11],
            0,
            "歌手（仅在本机核对，可选）",
        ),
        (0x81, ARTIST as u16, [12, 80, 258, 16], edit, ""),
        (
            0x80,
            IDCANCEL.0 as u16,
            [141, 113, 59, 19],
            WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
            "取消",
        ),
        (
            0x80,
            IDOK.0 as u16,
            [206, 113, 64, 19],
            WS_TABSTOP.0 | WS_DISABLED.0,
            "查这一首",
        ),
    ];
    let automatic_controls = [
        (
            0x82,
            AUTO_BODY as u16,
            [12, 10, 284, 66],
            0,
            "你用音乐软件听歌时，取词器可以自动把歌名和歌手发给网易云曲库或 LRCLIB，查到后显示在壁纸上。浏览器里的视频标题不会自动发送。\r\n\r\n这个选择随时可以在圆圈的右键菜单里修改。",
        ),
        (
            0x80,
            AUTO_ACCEPT as u16,
            [164, 84, 62, 20],
            WS_TABSTOP.0,
            "自动查词",
        ),
        (
            0x80,
            IDCANCEL.0 as u16,
            [234, 84, 62, 20],
            WS_TABSTOP.0,
            "暂不",
        ),
    ];
    let controls: &[_] = if automatic {
        &automatic_controls
    } else {
        &manual_controls
    };
    for &(class, id, rect, style, label) in controls {
        if !words.len().is_multiple_of(2) {
            words.push(0);
        }
        dword(&mut words, style | WS_CHILD.0 | WS_VISIBLE.0);
        dword(&mut words, 0);
        words.extend(rect);
        words.extend([id, 0xffff, class]);
        string(&mut words, label);
        words.push(0);
    }
    words
        .chunks(2)
        .map(|w| u32::from(w[0]) | (u32::from(*w.get(1).unwrap_or(&0)) << 16))
        .collect()
}

fn field(hwnd: HWND, id: i32) -> Option<String> {
    let control = unsafe { GetDlgItem(hwnd, id) }.ok()?;
    let size = unsafe { GetWindowTextLengthW(control) };
    if !(0..=240).contains(&size) {
        return None;
    }
    let mut buffer = vec![0; size as usize + 1];
    let read = unsafe { GetDlgItemTextW(hwnd, id, &mut buffer) } as usize;
    String::from_utf16(&buffer[..read])
        .ok()
        .map(|v| v.trim().to_owned())
}

fn fields(hwnd: HWND) -> Option<(String, String)> {
    let song = field(hwnd, TITLE)?;
    let artist = field(hwnd, ARTIST)?;
    valid_lookup_input(&song, &artist).then_some((song, artist))
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wparam: WPARAM, _: LPARAM) -> isize {
    let action = match msg {
        WM_CLOSE => IDCANCEL.0,
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            let notification = (wparam.0 >> 16) as u32;
            if matches!(id, TITLE | ARTIST) && notification == EN_CHANGE {
                if let Ok(button) = unsafe { GetDlgItem(hwnd, IDOK.0) } {
                    let _ = unsafe { EnableWindow(button, fields(hwnd).is_some()) };
                }
                return 1;
            }
            if notification != BN_CLICKED || (id != IDCANCEL.0 && id != IDOK.0 && id != AUTO_ACCEPT)
            {
                return 0;
            }
            id
        }
        _ => return 0,
    };
    let _ = unsafe { PostMessageW(hwnd, ANSWER, WPARAM(action as usize), LPARAM(0)) };
    1
}

pub struct LookupDialog {
    hwnd: HWND,
    id: Option<u64>,
    bridge: Arc<UiBridge>,
}

impl LookupDialog {
    pub fn open(owner: HWND, id: u64, title: &str, bridge: Arc<UiBridge>) -> Result<Self> {
        let dialog = Self::create(owner, Some(id), bridge)?;
        if !title.is_empty() && valid_lookup_input(title, "") {
            unsafe { SetDlgItemTextW(dialog.hwnd, TITLE, &HSTRING::from(title))? };
        }
        Ok(dialog)
    }
    pub fn automatic(owner: HWND, bridge: Arc<UiBridge>) -> Result<Self> {
        let dialog = Self::create(owner, None, bridge)?;
        unsafe {
            SendMessageW(dialog.hwnd, DM_SETDEFID, WPARAM(0), LPARAM(0));
            for id in [AUTO_ACCEPT, IDCANCEL.0] {
                let button = GetDlgItem(dialog.hwnd, id)?;
                SendMessageW(
                    button,
                    BM_SETSTYLE,
                    WPARAM(BS_PUSHBUTTON as usize),
                    LPARAM(0),
                );
            }
        }
        Ok(dialog)
    }
    fn create(owner: HWND, id: Option<u64>, bridge: Arc<UiBridge>) -> Result<Self> {
        let layout = template(id.is_none());
        let module = unsafe { GetModuleHandleW(None)? };
        let hwnd = unsafe {
            CreateDialogIndirectParamW(
                HINSTANCE(module.0),
                layout.as_ptr().cast(),
                owner,
                Some(dialog_proc),
                LPARAM(0),
            )?
        };
        Ok(Self { hwnd, id, bridge })
    }

    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(self.hwnd);
            if let Ok(control) =
                GetDlgItem(self.hwnd, if self.id.is_some() { TITLE } else { AUTO_BODY })
            {
                let _ = SetFocus(control);
            }
        }
    }

    pub fn current(&self) -> bool {
        self.id.map_or_else(
            || self.bridge.ask_auto_pending(),
            |id| self.bridge.lookup_offer_is_current(id),
        )
    }

    pub fn handle(&self, message: &MSG) -> bool {
        if message.hwnd != self.hwnd && !unsafe { IsChild(self.hwnd, message.hwnd) }.as_bool() {
            return false;
        }
        if message.hwnd == self.hwnd && message.message == ANSWER {
            if self.current() {
                if let Some(id) = self.id {
                    if message.wParam.0 == IDOK.0 as usize {
                        if let Some(answer) = fields(self.hwnd) {
                            self.bridge.answer_lookup(id, Some(answer));
                        }
                    } else if message.wParam.0 == IDCANCEL.0 as usize {
                        self.bridge.answer_lookup(id, None);
                    }
                } else if message.wParam.0 == AUTO_ACCEPT as usize
                    || message.wParam.0 == IDCANCEL.0 as usize
                {
                    save_choice(
                        self.hwnd,
                        &self.bridge,
                        message.wParam.0 == AUTO_ACCEPT as usize,
                    );
                }
            }
            return true;
        }
        unsafe { IsDialogMessageW(self.hwnd, message) }.as_bool()
    }
}

impl Drop for LookupDialog {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.bridge.answer_lookup(id, None);
        }
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

pub fn save_choice(hwnd: HWND, bridge: &UiBridge, enabled: bool) {
    if bridge.choose_auto(enabled).is_err() {
        unsafe {
            let _ = MessageBoxW(
                hwnd,
                windows::core::w!("无法保存设置，请检查设置目录的写入权限。"),
                windows::core::w!("雨夜轻息取词器"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

    fn open(bridge: &Arc<UiBridge>, id: u64, suggestion: &str) -> LookupDialog {
        bridge.offer_lookup_title(Some((id, suggestion.into())));
        LookupDialog::open(HWND::default(), id, suggestion, Arc::clone(bridge)).unwrap()
    }

    fn answer(dialog: &LookupDialog) {
        let mut message = MSG::default();
        assert!(
            unsafe { PeekMessageW(&mut message, dialog.hwnd, ANSWER, ANSWER, PM_REMOVE) }.as_bool()
        );
        assert!(dialog.handle(&message));
    }

    #[test]
    fn native_edit_requires_a_song_and_returns_only_edited_fields() {
        let bridge = Arc::new(UiBridge::default());
        let dialog = open(&bridge, 1, "");
        unsafe {
            let ok = GetDlgItem(dialog.hwnd, IDOK.0).unwrap();
            assert!(!IsWindowEnabled(ok).as_bool());
            assert_eq!(field(dialog.hwnd, TITLE).as_deref(), Some(""));
            assert_eq!(field(dialog.hwnd, ARTIST).as_deref(), Some(""));
            SetDlgItemTextW(dialog.hwnd, TITLE, &HSTRING::from("Silver Meadow")).unwrap();
            assert!(IsWindowEnabled(ok).as_bool());
            SetDlgItemTextW(dialog.hwnd, ARTIST, &HSTRING::from("Nerai")).unwrap();
            SetDlgItemTextW(dialog.hwnd, TITLE, &HSTRING::from("x".repeat(121))).unwrap();
            assert!(!IsWindowEnabled(ok).as_bool());
            SetDlgItemTextW(dialog.hwnd, TITLE, &HSTRING::from("Silver Meadow")).unwrap();
            SendMessageW(ok, BM_CLICK, WPARAM(0), LPARAM(0));
        }
        answer(&dialog);
        drop(dialog);
        assert_eq!(
            bridge.take_lookup_answer(),
            Some((1, Some(("Silver Meadow".into(), "Nerai".into()))))
        );
    }

    #[test]
    fn automatic_dialog_needs_an_explicit_click_and_records_close_as_declined() {
        for choice in [Some(true), Some(false), None] {
            let dir = std::env::temp_dir().join(format!(
                "rain-night-auto-dialog-{}-{choice:?}",
                std::process::id()
            ));
            let bridge = Arc::new(UiBridge::with_settings_dir(dir.clone()));
            bridge.set_lookup_context(false, true, true);
            assert!(!bridge.can_request_lookup());
            let dialog = LookupDialog::automatic(HWND::default(), Arc::clone(&bridge)).unwrap();
            assert_eq!(bridge.auto_policy().0, None);
            let revision = bridge.auto_revision();
            let initial = bridge.auto_policy().1;
            unsafe {
                let ok = GetDlgItem(dialog.hwnd, AUTO_ACCEPT).unwrap();
                let no = GetDlgItem(dialog.hwnd, IDCANCEL.0).unwrap();
                for button in [ok, no] {
                    assert_eq!(GetWindowLongW(button, GWL_STYLE) & 0xf, BS_PUSHBUTTON);
                }
                SendMessageW(dialog.hwnd, WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0));
                answer(&dialog);
                assert!(dialog.current());
                match choice {
                    Some(true) => {
                        SendMessageW(ok, BM_CLICK, WPARAM(0), LPARAM(0));
                    }
                    Some(false) => {
                        SendMessageW(no, BM_CLICK, WPARAM(0), LPARAM(0));
                    }
                    None => {
                        SendMessageW(dialog.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            answer(&dialog);
            assert!(!dialog.current());
            drop(dialog);
            let enabled = choice.unwrap_or(false);
            assert_eq!(bridge.auto_policy().0, Some(enabled));
            assert_eq!(bridge.can_request_lookup(), !enabled);
            assert_ne!(initial, revision.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(
                UiBridge::with_settings_dir(dir.clone()).auto_policy().0,
                Some(enabled)
            );
            if enabled {
                let old = bridge.auto_policy().1;
                bridge.choose_auto(false).unwrap();
                assert_ne!(old, revision.load(std::sync::atomic::Ordering::Acquire));
            }
            assert!(bridge.can_request_lookup());
            bridge.set_lookup_context(true, false, false);
            assert!(!bridge.ask_auto_pending());
            assert!(bridge.can_request_lookup());
            std::fs::remove_file(dir.join("settings.json")).unwrap();
            std::fs::remove_dir(dir).unwrap();
        }
    }

    #[test]
    fn dismiss_cancel_and_late_submit_never_authorize_a_new_offer() {
        let bridge = Arc::new(UiBridge::default());
        let first = open(&bridge, 2, "First");
        unsafe {
            SendMessageW(first.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        answer(&first);
        drop(first);
        assert_eq!(bridge.take_lookup_answer(), Some((2, None)));

        let old = open(&bridge, 3, "Second");
        unsafe {
            let ok = GetDlgItem(old.hwnd, IDOK.0).unwrap();
            SendMessageW(ok, BM_CLICK, WPARAM(0), LPARAM(0));
        }
        let fresh = open(&bridge, 4, "");
        let unrelated = MSG {
            hwnd: fresh.hwnd,
            message: WM_CHAR,
            ..Default::default()
        };
        assert!(!old.handle(&unrelated));
        answer(&old);
        drop(old);
        assert!(bridge.lookup_offer_is_current(4));
        assert_eq!(bridge.take_lookup_answer(), None);
        unsafe {
            let cancel = GetDlgItem(fresh.hwnd, IDCANCEL.0).unwrap();
            SendMessageW(cancel, BM_CLICK, WPARAM(0), LPARAM(0));
        }
        answer(&fresh);
        assert_eq!(bridge.take_lookup_answer(), Some((4, None)));
    }
}
