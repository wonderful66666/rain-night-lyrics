use crate::animation::{self, Anim};
use crate::desktop::{UiBridge, UiSnapshot};
use crate::placement::{Anchor, Placement, disc_dpi};
use crate::render::{CANVAS, Frame, Renderer};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW,
    SetWaitableTimerEx, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, Owned, PCWSTR, Result, w};

const SNAPSHOT: u32 = WM_APP + 1;
const RESTACK: u32 = WM_APP + 2;
const INPUT: u32 = WM_APP + 3;
const DISPLAY: u32 = WM_APP + 4;
const REDRAW: u32 = WM_APP + 5;
const CLOSE: u32 = WM_APP + 6;
const MENU_WAKE: usize = 1;
const TARGET_HZ: PCWSTR = w!("RainNightDisc.TargetMilliHz");
const DRAW_HZ: PCWSTR = w!("RainNightDisc.DrawMilliHz");
const LEAVE: u32 = 0x02a3;
const LEFT: f64 = 40.0;
const TOP: f64 = 24.0;

#[derive(Clone, Copy, Default)]
struct Hit {
    origin: (i32, i32),
    dpi: u32,
    width: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Outside,
    Disc,
    Body,
    Close,
}

impl Hit {
    fn part(self, point: (i32, i32)) -> Part {
        if self.dpi == 0 || self.width < 1.0 {
            return Part::Outside;
        }
        let units = 96.0 / self.dpi as f64;
        let x = (point.0 - self.origin.0) as f64 * units - LEFT;
        let y = (point.1 - self.origin.1) as f64 * units - TOP;
        let half = (self.width * 0.5).min(22.0);
        let nearest = x.clamp(half, self.width - half);
        if (x - nearest).powi(2) + (y - 22.0).powi(2) > 22.0 * 22.0 {
            return Part::Outside;
        }
        if x <= 44.0 {
            return Part::Disc;
        }
        let right = self.width - 5.0;
        let left = right - 71.0;
        if x >= left
            && self.width >= 180.0
            && (x - x.clamp(left + 17.0, right - 17.0)).powi(2) + (y - 22.0).powi(2) <= 17.0 * 17.0
        {
            Part::Close
        } else {
            Part::Body
        }
    }
}

thread_local! {
    static HIT:Cell<Hit>=Cell::new(Hit::default());
    static WINDOW:Cell<HWND>=Cell::new(HWND::default());
    static CONTROLS:Cell<(HANDLE,HANDLE)>=Cell::new((HANDLE::default(),HANDLE::default()));
}

fn point_lparam(p: LPARAM) -> (i32, i32) {
    (
        (p.0 as u16 as i16) as i32,
        ((p.0 >> 16) as u16 as i16) as i32,
    )
}

fn cursor() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }.ok()?;
    Some((p.x, p.y))
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let (forward, wparam) = match msg {
        WM_TIMER if wparam.0 == MENU_WAKE => {
            use windows::Win32::System::Threading::{SetEvent, WaitForSingleObject};
            let (stop, show) = CONTROLS.get();
            unsafe {
                if WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 {
                    let _ = EndMenu();
                } else if WaitForSingleObject(show, 0) == WAIT_OBJECT_0 {
                    let _ = EndMenu();
                    let _ = SetEvent(show);
                }
            }
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => {
            return LRESULT(if HIT.get().part(point_lparam(lparam)) == Part::Outside {
                HTTRANSPARENT
            } else {
                HTCLIENT as i32
            } as isize);
        }
        WM_CLOSE => (CLOSE, WPARAM(0)),
        WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => (DISPLAY, WPARAM(0)),
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP | LEAVE | WM_CAPTURECHANGED => {
            (INPUT, WPARAM(msg as usize))
        }
        WM_PAINT => {
            let _ = unsafe { ValidateRect(hwnd, None) };
            (REDRAW, WPARAM(0))
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    };
    let _ = unsafe { PostMessageW(hwnd, forward, wparam, lparam) };
    LRESULT(0)
}

unsafe extern "system" fn shell_event(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    let hwnd = WINDOW.get();
    if !hwnd.is_invalid() {
        let _ = unsafe { PostMessageW(hwnd, RESTACK, WPARAM(0), LPARAM(0)) };
    }
}

fn desktop_window(hwnd: HWND) -> bool {
    let mut name = [0u16; 64];
    let count = unsafe { GetClassNameW(hwnd, &mut name) } as usize;
    let name = String::from_utf16_lossy(&name[..count]);
    name == "Progman" || name == "WorkerW"
}

fn restack(hwnd: HWND) {
    #[cfg(test)]
    if tests::fixture_active() {
        return;
    }
    unsafe {
        let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
        let fg = GetForegroundWindow();
        if desktop_window(fg) {
            let _ = SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, flags);
            return;
        }
        let mut below = HWND_TOP;
        let mut next = GetTopWindow(None);
        while let Ok(other) = next {
            next = GetWindow(other, GW_HWNDNEXT);
            if other == hwnd || !IsWindowVisible(other).as_bool() {
                continue;
            }
            if desktop_window(other)
                && FindWindowExW(other, None, w!("SHELLDLL_DefView"), None).is_ok()
            {
                let _ = SetWindowPos(hwnd, below, 0, 0, 0, 0, flags);
                return;
            }
            if GetWindowLongPtrW(other, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize == 0 {
                below = other;
            }
        }
        if !fg.is_invalid()
            && fg != hwnd
            && GetWindowLongPtrW(fg, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize == 0
        {
            let _ = SetWindowPos(hwnd, fg, 0, 0, 0, 0, flags);
        } else {
            let _ = SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, flags);
        }
    }
}

fn covered(hwnd: HWND, origin: (i32, i32), dpi: u32) -> bool {
    #[cfg(test)]
    if tests::fixture_active() {
        return false;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() || fg == hwnd || desktop_window(fg) || unsafe { IsIconic(fg) }.as_bool() {
        return false;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(fg, &mut rect) }.is_err() {
        return false;
    }
    rect.left <= origin.0
        && rect.top <= origin.1
        && rect.right >= origin.0 + pixel(CANVAS.0 as f64, dpi)
        && rect.bottom >= origin.1 + pixel(CANVAS.1 as f64, dpi)
}

fn reduced_motion() -> bool {
    let mut enabled = BOOL(1);
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
        && !enabled.as_bool()
}
fn pixel(units: f64, dpi: u32) -> i32 {
    (units * dpi as f64 / 96.0).round() as i32
}

pub fn frame_stats() -> Option<(f64, f64)> {
    let hwnd =
        unsafe { FindWindowW(w!("RainNightLyrics.DesktopDisc"), w!("雨夜轻息取词器")) }.ok()?;
    let read = |key| unsafe { GetPropW(hwnd, key) }.0 as usize as f64 / 1000.0;
    let hz = read(TARGET_HZ);
    (hz > 0.0).then(|| (hz, read(DRAW_HZ)))
}

#[derive(Default)]
struct Meter {
    target: f64,
    start: Option<f64>,
    frames: u32,
}
impl Meter {
    fn tick(&mut self, hwnd: HWND, now: f64, hz: f64, running: bool, painted: bool) {
        let publish = |key, value: f64| unsafe {
            let _ = SetPropW(
                hwnd,
                key,
                HANDLE((value * 1000.0).round() as usize as *mut _),
            );
        };
        if self.target != hz {
            self.target = hz;
            publish(TARGET_HZ, hz);
            self.start = None;
            self.frames = 0;
            publish(DRAW_HZ, 0.0);
        }
        if !running {
            if self.start.take().is_some() {
                self.frames = 0;
                publish(DRAW_HZ, 0.0);
            }
        } else if painted {
            let Some(s) = self.start else {
                self.start = Some(now);
                return;
            };
            self.frames += 1;
            if now - s >= 2.0 {
                publish(DRAW_HZ, self.frames as f64 / (now - s));
                self.frames = 0;
                self.start = Some(now);
            }
        }
    }
}

struct DeadlineTimer {
    handle: Owned<HANDLE>,
    deadline: Option<f64>,
}
impl DeadlineTimer {
    fn new() -> Result<Self> {
        let create =
            |flags| unsafe { CreateWaitableTimerExW(None, None, flags, TIMER_ALL_ACCESS.0) };
        let handle = create(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION).or_else(|_| create(0))?;
        Ok(Self {
            handle: unsafe { Owned::new(handle) },
            deadline: None,
        })
    }
    fn set(&mut self, now: f64, deadline: Option<f64>) -> Result<()> {
        if self.deadline == deadline {
            return Ok(());
        }
        if let Some(d) = deadline {
            let ticks = -((d - now) * 10_000_000.0).ceil().max(1.0) as i64;
            unsafe { SetWaitableTimerEx(*self.handle, &ticks, 0, None, None, None, 0) }?;
        } else if self.deadline.is_some() {
            unsafe { CancelWaitableTimer(*self.handle) }?;
        }
        self.deadline = deadline;
        Ok(())
    }
}

struct Move {
    from: (i32, i32),
    to: (i32, i32),
    since: f64,
}
impl Move {
    fn at(&self, t: f64) -> (i32, i32) {
        if self.from == self.to {
            return self.to;
        }
        let k = animation::out((t - self.since) / 0.220);
        (
            (self.from.0 as f64 + (self.to.0 - self.from.0) as f64 * k).round() as i32,
            (self.from.1 as f64 + (self.to.1 - self.from.1) as f64 * k).round() as i32,
        )
    }
}

struct Press {
    part: Part,
    cursor: (i32, i32),
    original: (i32, i32),
    offset: (i32, i32),
    dragged: bool,
}

struct Overlay {
    window: HWND,
    view: UiSnapshot,
    bridge: Arc<UiBridge>,
    placement: Placement,
    anchor: Anchor,
    motion: Move,
    anim: Anim,
    renderer: Renderer,
    origin: (i32, i32),
    close_hover: bool,
    press: Option<Press>,
    was_down: bool,
    clock: f64,
    sampled: f64,
}
impl Overlay {
    fn layout(&mut self, t: f64, force: bool) {
        if self.press.is_some() || (!force && self.anim.hovered) {
            return;
        }
        let next = self.placement.anchor(&self.view);
        let to = (next.x, next.y);
        if self.motion.to != to || next.dpi != self.anchor.dpi {
            let from = if self.anim.reduced
                || next.key != self.anchor.key
                || next.dpi != self.anchor.dpi
            {
                to
            } else {
                self.motion.at(t)
            };
            self.motion = Move { from, to, since: t };
        }
        self.bridge
            .cover_size(pixel(34.0, disc_dpi(next.dpi)).max(1) as u32);
        self.anchor = next;
    }
    fn sync_clock(&mut self, t: f64) {
        self.sampled = t;
        self.clock = self.view.media.as_ref().map_or(0.0, |m| {
            let elapsed = (crate::media::now_ms().saturating_sub(m.sent)).max(0) as f64 / 1000.0;
            m.position.unwrap_or(0.0) + if m.playing { elapsed * m.rate } else { 0.0 }
        });
    }
    fn snapshot(&mut self, t: f64) -> bool {
        let new = self.bridge.latest();
        let changed = self.view.status != new.status
            || match (&self.view.cover, &new.cover) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (None, None) => false,
                _ => true,
            }
            || match (&self.view.media, &new.media) {
                (Some(a), Some(b)) => {
                    a.song != b.song
                        || a.playing != b.playing
                        || a.duration != b.duration
                        || a.position != b.position
                        || a.rate != b.rate
                }
                (None, None) => false,
                _ => true,
            };
        self.view = new;
        let playing = self.view.media.as_ref().is_some_and(|m| m.playing);
        self.anim.playback(playing, t);
        self.sync_clock(t);
        let old = (self.motion.to, self.anchor.dpi);
        self.layout(t, false);
        changed || old != (self.motion.to, self.anchor.dpi)
    }
    fn draw(&mut self, t: f64) -> Result<()> {
        let p = self.anim.pose(t);
        let dpi = disc_dpi(self.anchor.dpi);
        let circle = self.press.as_ref().filter(|p| p.dragged).map_or_else(
            || self.motion.at(t),
            |p| (p.original.0 + p.offset.0, p.original.1 + p.offset.1),
        );
        self.origin = (circle.0 - pixel(LEFT, dpi), circle.1 - pixel(TOP, dpi));
        let playing = self.view.media.as_ref().is_some_and(|m| m.playing);
        let progress = self
            .view
            .media
            .as_ref()
            .filter(|m| m.duration > 0.0 && m.position.is_some())
            .map(|m| {
                ((self.clock
                    + if playing {
                        (t - self.sampled) * m.rate
                    } else {
                        0.0
                    })
                    / m.duration)
                    .clamp(0.0, 1.0) as f32
            });
        let frame = Frame {
            width: p.width,
            opacity: p.opacity,
            dx: p.dx,
            dy: p.dy,
            scale: p.scale,
            angle: p.angle,
            progress,
            playing,
            has_media: self.view.media.is_some(),
            cover: self.view.cover.as_deref(),
            status: self.view.status,
            hover: self.anim.hovered,
            close_hover: self.close_hover,
            content: p.content,
            tooltip: p.tip,
            breath: p.breath,
            ripple: p.ripple,
            splash: p.splash,
        };
        if let Err(e) = self.renderer.draw(self.window, self.origin, dpi, &frame) {
            self.renderer = Renderer::new().map_err(|_| e)?;
            self.renderer.draw(self.window, self.origin, dpi, &frame)?;
        }
        HIT.set(Hit {
            origin: self.origin,
            dpi,
            width: if self.anim.interactive(t) {
                p.width as f64
            } else {
                0.0
            },
        });
        #[cfg(test)]
        tests::observe(self, t, &p);
        Ok(())
    }
    fn menu(&mut self, point: (i32, i32), t: f64) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let opened = Instant::now();
            let _ = AppendMenuW(menu, MF_STRING, 1, w!("回到默认位置"));
            let _ = AppendMenuW(menu, MF_STRING, 2, w!("关闭取词器"));
            SetTimer(self.window, MENU_WAKE, 100, None);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.0,
                point.1,
                0,
                self.window,
                None,
            )
            .0;
            let _ = KillTimer(self.window, MENU_WAKE);
            let _ = DestroyMenu(menu);
            let t = t + opened.elapsed().as_secs_f64();
            self.snapshot(t);
            if selected == 1 {
                self.placement.reset(&self.anchor);
                self.layout(t, true);
            }
            if selected == 2 {
                self.anim.close(t);
            }
        }
    }
    fn input(&mut self, msg: u32, p: LPARAM, t: f64) {
        if !self.anim.interactive(t) {
            return;
        }
        let client = point_lparam(p);
        let point = (self.origin.0 + client.0, self.origin.1 + client.1);
        let part = HIT.get().part(point);
        match msg {
            WM_MOUSEMOVE => {
                if let Some(press) = &mut self.press {
                    press.offset = (point.0 - press.cursor.0, point.1 - press.cursor.1);
                    let limit = 4.0 * self.anchor.dpi as f64 / 96.0;
                    press.dragged |= (press.offset.0 as f64).hypot(press.offset.1 as f64) > limit;
                }
                if !self.anim.hovered
                    && part != Part::Outside
                    && unsafe { IsWindowVisible(self.window) }.as_bool()
                {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.window,
                        ..Default::default()
                    };
                    let _ = unsafe { TrackMouseEvent(&mut track) };
                }
                self.anim.hover(part != Part::Outside, t);
                self.close_hover = part == Part::Close;
            }
            LEAVE => {
                self.anim.hover(false, t);
                self.close_hover = false;
            }
            WM_LBUTTONDOWN if part != Part::Outside => {
                self.was_down = true;
                self.press = Some(Press {
                    part,
                    cursor: point,
                    original: self.motion.at(t),
                    offset: (0, 0),
                    dragged: false,
                });
                if unsafe { IsWindowVisible(self.window) }.as_bool() {
                    unsafe { SetCapture(self.window) };
                }
            }
            WM_LBUTTONUP => {
                let press = self.press.take();
                if unsafe { GetCapture() } == self.window {
                    let _ = unsafe { ReleaseCapture() };
                }
                if let Some(press) = press {
                    if press.dragged {
                        self.placement
                            .drag(&self.anchor, press.offset.0, press.offset.1);
                        let pos = (
                            press.original.0 + press.offset.0,
                            press.original.1 + press.offset.1,
                        );
                        self.motion = Move {
                            from: pos,
                            to: pos,
                            since: t,
                        };
                        self.layout(t, true);
                    } else if part == press.part {
                        if self.anim.expanded && part == Part::Close {
                            self.anim.close(t);
                        } else if part == Part::Disc {
                            self.anim.set_expanded(!self.anim.expanded, t);
                        }
                    }
                }
                self.was_down = false;
            }
            WM_CAPTURECHANGED => self.press = None,
            WM_RBUTTONUP if part != Part::Outside => self.menu(point, t),
            _ => {}
        }
    }
}

struct Resources {
    window: HWND,
    hooks: Vec<HWINEVENTHOOK>,
    previous_dpi: windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT,
}
impl Drop for Resources {
    fn drop(&mut self) {
        WINDOW.set(HWND::default());
        CONTROLS.set((HANDLE::default(), HANDLE::default()));
        HIT.set(Hit::default());
        unsafe {
            for hook in self.hooks.drain(..) {
                let _ = UnhookWinEvent(hook);
            }
            if !self.window.is_invalid() {
                let _ = RemovePropW(self.window, TARGET_HZ);
                let _ = RemovePropW(self.window, DRAW_HZ);
                let _ = DestroyWindow(self.window);
            }
            if !self.previous_dpi.is_invalid() {
                SetThreadDpiAwarenessContext(self.previous_dpi);
            }
            CoUninitialize();
        }
    }
}
pub fn run(bridge: Arc<UiBridge>, stop: HANDLE, show: HANDLE) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    let mut sys = Resources {
        window: HWND::default(),
        hooks: Vec::new(),
        previous_dpi: unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        },
    };
    let class = w!("RainNightLyrics.DesktopDisc");
    let module = unsafe { GetModuleHandleW(None)? };
    let instance = HINSTANCE(module.0);
    let wc = WNDCLASSW {
        hInstance: instance,
        lpszClassName: class,
        lpfnWndProc: Some(window_proc),
        hCursor: unsafe { LoadCursorW(None, IDC_HAND) }?,
        ..Default::default()
    };
    unsafe {
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_win32());
        }
    }
    let view = bridge.latest();
    let mut placement = Placement::new();
    let anchor = placement.anchor(&view);
    let dpi = disc_dpi(anchor.dpi);
    let origin = (anchor.x - pixel(LEFT, dpi), anchor.y - pixel(TOP, dpi));
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("雨夜轻息取词器"),
            WS_POPUP,
            origin.0,
            origin.1,
            pixel(CANVAS.0 as f64, dpi),
            pixel(CANVAS.1 as f64, dpi),
            None,
            None,
            instance,
            None,
        )?
    };
    sys.window = hwnd;
    WINDOW.set(hwnd);
    CONTROLS.set((stop, show));
    bridge.attach(hwnd);
    bridge.cover_size(pixel(34.0, dpi) as u32);
    let pos = (anchor.x, anchor.y);
    let mut app = Overlay {
        window: hwnd,
        view,
        bridge,
        placement,
        anchor,
        motion: Move {
            from: pos,
            to: pos,
            since: 0.0,
        },
        anim: Anim::new(reduced_motion()),
        renderer: Renderer::new()?,
        origin,
        close_hover: false,
        press: None,
        was_down: false,
        sampled: 0.0,
        clock: 0.0,
    };
    let start = Instant::now();
    app.snapshot(0.0);
    for event in [
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MOVESIZEEND,
        EVENT_SYSTEM_MINIMIZEEND,
    ] {
        #[cfg(test)]
        if tests::fixture_active() {
            break;
        }
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(shell_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if !hook.is_invalid() {
            sys.hooks.push(hook);
        }
    }
    app.draw(0.0)?;
    restack(hwnd);
    let mut dirty = true;
    let mut timer = DeadlineTimer::new()?;
    let mut meter = Meter::default();
    let mut obscured = covered(hwnd, app.origin, disc_dpi(app.anchor.dpi));
    loop {
        let t = start.elapsed().as_secs_f64();
        let was_expanded = app.anim.expanded;
        app.anim.advance(t);
        dirty |= was_expanded != app.anim.expanded;
        if app.anim.finished(t) {
            break;
        }
        if app.anim.expanded && app.press.is_none() && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
            if down
                && !app.was_down
                && cursor().is_some_and(|p| {
                    HIT.get().part(p) == Part::Outside
                        || unsafe { WindowFromPoint(POINT { x: p.0, y: p.1 }) } != hwnd
                })
            {
                app.anim.set_expanded(false, t);
                dirty = true;
            }
            app.was_down = down;
        }
        if dirty && !obscured {
            app.layout(t, false);
            app.draw(t)?;
        }
        let now = start.elapsed().as_secs_f64();
        let hz = app.anchor.hz;
        let moving = app.motion.from != app.motion.to && t - app.motion.since < 0.220;
        let animating = !obscured && (moving || app.anim.animating(t, app.view.media.is_some()));
        meter.tick(hwnd, now, hz, animating, dirty && !obscured);
        dirty = false;
        let paint_at = if animating {
            Some(animation::next_frame(now, hz))
        } else if obscured {
            None
        } else if app.anim.reduced
            && app
                .view
                .media
                .as_ref()
                .is_some_and(|m| m.playing && m.duration > 0.0 && m.position.is_some())
        {
            Some(animation::next_frame(now, 1.0))
        } else {
            app.anim.hint_deadline(t)
        };
        let outside_at = (app.anim.expanded && !app.anim.hovered && !obscured)
            .then(|| animation::next_frame(now, 60.0));
        let finish = app
            .anim
            .closing
            .map(|at| at + if app.anim.reduced { 0.2 } else { 1.3 });
        let deadline = [paint_at, outside_at, app.anim.collapse_at, finish]
            .into_iter()
            .flatten()
            .min_by(f64::total_cmp);
        timer.set(start.elapsed().as_secs_f64(), deadline)?;
        let closing = app.anim.closing.is_some();
        let mut handles = [HANDLE::default(); 3];
        let mut n = 0;
        if !closing {
            handles[0] = stop;
            handles[1] = show;
            n = 2;
        }
        let timer_index = n;
        if deadline.is_some() {
            handles[n] = *timer.handle;
            n += 1;
        }
        let result = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&handles[..n]),
                u32::MAX,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        let t = start.elapsed().as_secs_f64();
        if result == WAIT_FAILED {
            return Err(Error::from_win32());
        }
        if deadline.is_some() && result.0 == WAIT_OBJECT_0.0 + timer_index as u32 {
            timer.deadline = None;
            dirty = paint_at.is_some_and(|at| at <= t + 0.0001);
        } else if !closing && result == WAIT_OBJECT_0 {
            app.anim.close(t);
            dirty = true;
        } else if !closing && result.0 == WAIT_OBJECT_0.0 + 1 {
            app.anim.reveal(t);
            restack(hwnd);
            dirty = true;
        }
        let mut m = MSG::default();
        while unsafe { PeekMessageW(&mut m, None, 0, 0, PM_REMOVE) }.as_bool() {
            if m.message == WM_QUIT {
                app.anim.close(t);
                dirty = true;
                continue;
            }
            if m.hwnd == hwnd {
                match m.message {
                    SNAPSHOT => {
                        dirty |= app.snapshot(t) && !obscured;
                        continue;
                    }
                    RESTACK => {
                        restack(hwnd);
                        obscured = covered(hwnd, app.origin, disc_dpi(app.anchor.dpi));
                        dirty = true;
                        continue;
                    }
                    INPUT => {
                        app.input(m.wParam.0 as u32, m.lParam, start.elapsed().as_secs_f64());
                        dirty = true;
                        continue;
                    }
                    DISPLAY => {
                        app.placement.invalidate();
                        app.anim.reduced = reduced_motion();
                        app.layout(t, true);
                        obscured = covered(hwnd, app.origin, disc_dpi(app.anchor.dpi));
                        dirty = true;
                        continue;
                    }
                    REDRAW => {
                        dirty = true;
                        continue;
                    }
                    CLOSE => {
                        app.anim.close(t);
                        dirty = true;
                        continue;
                    }
                    _ => {}
                }
            }
            unsafe {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
    }
    app.bridge.attach(HWND::default());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        sync::mpsc::{self, Receiver, Sender},
        thread,
        time::Duration,
    };
    use windows::Win32::System::Threading::{CreateEventW, SetEvent};
    use windows::core::Owned;

    #[derive(Debug)]
    struct Sample {
        window: usize,
        dpi: u32,
        ready: bool,
        expanded: bool,
        closing: bool,
        reduced: bool,
        width: f32,
        tooltip: f32,
        idle: bool,
    }

    thread_local! {static OBSERVER:RefCell<Option<Sender<Sample>>>=const {RefCell::new(None)};}
    pub(super) fn fixture_active() -> bool {
        OBSERVER.with_borrow(|o| o.is_some())
    }
    pub(super) fn observe(app: &Overlay, t: f64, p: &animation::Pose) {
        OBSERVER.with_borrow(|o| {
            if let Some(tx) = o {
                let _ = tx.send(Sample {
                    window: app.window.0 as usize,
                    dpi: disc_dpi(app.anchor.dpi),
                    ready: app.anim.interactive(t),
                    reduced: app.anim.reduced,
                    expanded: app.anim.expanded,
                    closing: app.anim.closing.is_some(),
                    width: p.width,
                    tooltip: p.tip,
                    idle: !app.anim.animating(t, app.view.media.is_some())
                        && app.anim.hint_deadline(t).is_none(),
                });
            }
        });
    }
    #[track_caller]
    fn until(rx: &Receiver<Sample>, predicate: impl Fn(&Sample) -> bool) -> Sample {
        let end = Instant::now() + Duration::from_secs(5);
        let mut last = None;
        loop {
            let Ok(s) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) else {
                panic!("hidden window did not reach the expected state; last frame {last:?}");
            };
            if predicate(&s) {
                return s;
            }
            last = Some(s);
        }
    }
    fn input(sample: &Sample, msg: u32, x: f64, y: f64) {
        let point = LPARAM(
            (pixel(LEFT + x, sample.dpi) & 0xffff) as isize
                | ((pixel(TOP + y, sample.dpi) as isize & 0xffff) << 16),
        );
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let result = PostMessageW(HWND(sample.window as *mut _), msg, WPARAM(0), point);
            SetThreadDpiAwarenessContext(previous);
            result.unwrap();
        }
    }
    #[test]
    fn private_layered_window_handles_mouse_show_and_stop() {
        for click_close in [true, false] {
            let stop = unsafe { Owned::new(CreateEventW(None, true, false, None).unwrap()) };
            let show = unsafe { Owned::new(CreateEventW(None, false, false, None).unwrap()) };
            let raw = (stop.0 as usize, show.0 as usize);
            let (tx, rx) = mpsc::channel();
            let bridge = Arc::new(UiBridge::default());
            let runner = Arc::clone(&bridge);
            let worker = thread::spawn(move || {
                OBSERVER.with_borrow_mut(|observer| *observer = Some(tx));
                let mut view = UiSnapshot::default();
                view.media = Some(crate::media::Media {
                    song: crate::media::Song {
                        title: "Test".into(),
                        artist: String::new(),
                        source: crate::media::Source::Other,
                    },
                    position: Some(1.0),
                    duration: 100.0,
                    rate: 1.0,
                    playing: false,
                    sent: crate::media::now_ms(),
                });
                runner.publish(view);
                run(runner, HANDLE(raw.0 as *mut _), HANDLE(raw.1 as *mut _))
            });
            let first = until(&rx, |s| s.ready && s.idle);
            let hwnd = HWND(first.window as *mut _);
            assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
            assert_ne!(unsafe { GetForegroundWindow() }, hwnd);
            assert!(rx.recv_timeout(Duration::from_millis(120)).is_err());
            if click_close {
                input(&first, WM_MOUSEMOVE, 22.0, 22.0);
                let tip = until(&rx, |s| s.tooltip > 0.95);
                input(&tip, WM_LBUTTONDOWN, 22.0, 22.0);
                input(&tip, WM_LBUTTONUP, 22.0, 22.0);
            } else {
                if !first.reduced {
                    let mut playing = bridge.latest();
                    playing.media.as_mut().unwrap().playing = true;
                    bridge.publish(playing);
                    let started = Instant::now();
                    until(&rx, |_| started.elapsed() >= Duration::from_millis(2150));
                    let target = unsafe { GetPropW(hwnd, TARGET_HZ) }.0 as usize as f64 / 1000.0;
                    let measured = unsafe { GetPropW(hwnd, DRAW_HZ) }.0 as usize as f64 / 1000.0;
                    eprintln!(
                        "Private layered HWND: target {target:.2} Hz, measured {measured:.2} draws/s."
                    );
                    assert!(target >= 1.0 && measured > 0.0);
                }
                unsafe {
                    SetEvent(*show).unwrap();
                }
            }
            let open = until(&rx, |s| s.expanded && s.width >= 214.5);
            assert!(open.tooltip <= 0.001);
            let began = Instant::now();
            if click_close {
                input(&open, WM_LBUTTONDOWN, 182.0, 22.0);
                input(&open, WM_LBUTTONUP, 182.0, 22.0);
            } else {
                unsafe {
                    SetEvent(*stop).unwrap();
                }
            }
            until(&rx, |s| s.closing);
            worker.join().unwrap().unwrap();
            let elapsed = began.elapsed().as_secs_f64();
            assert!(elapsed >= if open.reduced { 0.18 } else { 1.26 } && elapsed < 4.0);
            assert!(!unsafe { IsWindow(hwnd) }.as_bool());
        }
    }
    #[test]
    fn transparent_padding_shadow_and_round_corners_pass_clicks_through() {
        let h = Hit {
            origin: (-500, 40),
            dpi: 144,
            width: 216.0,
        };
        let p = |x: f64, y: f64| {
            h.part((
                h.origin.0 + pixel(LEFT + x, h.dpi),
                h.origin.1 + pixel(TOP + y, h.dpi),
            ))
        };
        assert_eq!(p(0.0, 0.0), Part::Outside);
        assert_eq!(p(22.0, 52.0), Part::Outside);
        assert_eq!(p(22.0, 22.0), Part::Disc);
        assert_eq!(p(175.0, 22.0), Part::Close);
        assert_eq!(p(100.0, 22.0), Part::Body);
    }
}
