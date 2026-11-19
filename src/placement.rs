use crate::desktop::{UiSnapshot, data_dir};
use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::{
    BOOL, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LPARAM, POINT, RECT,
};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplayMonitors,
    EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EDD_GET_DEVICE_INTERFACE_NAME, GetCursorPos, GetSystemMetrics, MONITORINFOF_PRIMARY,
    SM_CXSCREEN, SM_CYSCREEN,
};
use windows::core::{Interface, PCWSTR, w};

#[derive(Clone, Default, Debug, PartialEq)]
pub struct CapsuleData {
    pub display: Option<String>,
    pub paths: Vec<String>,
    pub fonts: Option<PathBuf>,
    pub title: String,
    pub subtitle: String,
    pub notice: Option<(String, String)>,
    pub font: u8,
    pub unknown_width: bool,
}

#[derive(Clone, Debug)]
pub struct Anchor {
    pub x: i32,
    pub y: i32,
    pub dpi: u32,
    pub hz: f64,
    pub key: String,
    pub docked: bool,
    pub animating: bool,
    base: (i32, i32),
    bounds: RECT,
}

impl Anchor {
    pub fn constrain_drag(&self, (x, y): (i32, i32)) -> (i32, i32) {
        (
            x.clamp(self.bounds.left, self.bounds.right),
            y.clamp(self.bounds.top, self.bounds.bottom),
        )
    }
}

struct Monitor {
    rect: RECT,
    dpi: u32,
    hz: f64,
    primary: bool,
    name: String,
    interfaces: Vec<String>,
    key: String,
}

fn utf16(buf: &[u16]) -> String {
    String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap_or(buf.len())])
}

pub fn disc_dpi(dpi: u32) -> u32 {
    ((u64::from(dpi) * 36 + 22) / 44).max(1) as u32
}

fn active_refresh_rates() -> BTreeMap<String, f64> {
    let mut rates = BTreeMap::new();
    let flags = QDC_ONLY_ACTIVE_PATHS;
    for _ in 0..3 {
        let (mut npaths, mut nmodes) = (0, 0);
        if unsafe { GetDisplayConfigBufferSizes(flags, &mut npaths, &mut nmodes) } != ERROR_SUCCESS
            || npaths == 0
        {
            break;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); npaths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nmodes as usize];
        let rc = unsafe {
            QueryDisplayConfig(
                flags,
                &mut npaths,
                paths.as_mut_ptr(),
                &mut nmodes,
                modes.as_mut_ptr(),
                None,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if rc != ERROR_SUCCESS {
            break;
        }
        for path in paths.iter().take(npaths as usize) {
            let rate = path.targetInfo.refreshRate;
            if rate.Denominator == 0 || rate.Numerator == 0 {
                continue;
            }
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0 {
                rates
                    .entry(display_key(&utf16(&source.viewGdiDeviceName)))
                    .or_insert(f64::from(rate.Numerator) / f64::from(rate.Denominator));
            }
        }
        break;
    }
    rates
}

fn anonymous_key(name: &str) -> String {
    let hash = name.bytes().fold(0xcbf29ce484222325u64, |hash, b| {
        (hash ^ u64::from(b.to_ascii_lowercase())).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

unsafe extern "system" fn collect_monitor(
    handle: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if !unsafe { GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
        return BOOL(1);
    }
    let name = utf16(&info.szDevice);
    let mut interfaces = Vec::new();
    for index in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplayDevicesW(
                PCWSTR(info.szDevice.as_ptr()),
                index,
                &mut device,
                EDD_GET_DEVICE_INTERFACE_NAME,
            )
        }
        .as_bool()
        {
            break;
        }
        let id = utf16(&device.DeviceID);
        if !id.is_empty() {
            interfaces.push(id);
        }
    }
    interfaces.sort_unstable();
    let key = anonymous_key(interfaces.first().unwrap_or(&name));
    let (mut x, mut y) = (96, 96);
    let _ = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let have_mode = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    }
    .as_bool();
    list.push(Monitor {
        rect: info.monitorInfo.rcMonitor,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        dpi: x.max(1),
        hz: if have_mode && mode.dmDisplayFrequency > 1 {
            f64::from(mode.dmDisplayFrequency)
        } else {
            60.0
        },
        interfaces,
        name,
        key,
    });
    BOOL(1)
}

fn monitors() -> Vec<Monitor> {
    let mut list = Vec::<Monitor>::new();
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&mut list as *mut Vec<Monitor>) as isize),
        )
    };
    let rates = active_refresh_rates();
    for monitor in &mut list {
        if let Some(rate) = rates.get(&display_key(&monitor.name)) {
            monitor.hz = *rate
        }
    }
    if list.is_empty() {
        list.push(Monitor {
            rect: RECT {
                left: 0,
                top: 0,
                right: unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1),
                bottom: unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1),
            },
            primary: true,
            dpi: 96,
            hz: 60.0,
            name: String::new(),
            interfaces: vec![],
            key: anonymous_key("primary"),
        });
    }
    list
}

fn display_key(name: &str) -> String {
    name.trim()
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .trim_start_matches(r"\\.\")
        .to_ascii_lowercase()
}

pub fn display_paths(settings: &serde_json::Value, selected: &str) -> Vec<String> {
    let Some(id) = display_key(selected)
        .strip_prefix("monitor")
        .and_then(|n| n.parse::<u64>().ok())
    else {
        return vec![];
    };
    let Some(map) = settings
        .pointer("/user/monitormap")
        .and_then(serde_json::Value::as_object)
    else {
        return vec![];
    };
    let mut matches: Vec<_> = map
        .iter()
        .filter(|(_, value)| value["location"].as_u64() == Some(id))
        .map(|(path, value)| {
            (
                std::cmp::Reverse(value["timestamp"].as_u64().unwrap_or_default()),
                path.clone(),
            )
        })
        .collect();
    matches.sort_unstable();
    matches.into_iter().map(|(_, path)| path).collect()
}

fn monitor_index(list: &[Monitor], id: &str) -> Option<usize> {
    let id = display_key(id);
    if id.is_empty() {
        return None;
    }
    if let Some(index) = id
        .strip_prefix("monitor")
        .and_then(|n| n.parse::<usize>().ok())
    {
        return (index < list.len()).then_some(index);
    }
    list.iter().position(|m| {
        display_key(&m.name) == id || m.interfaces.iter().any(|path| display_key(path) == id)
    })
}

fn text_size(text: &str, base: f32) -> f32 {
    let cjk = text.chars().any(|c| matches!(c as u32, 0x3000..=0x30ff | 0x3400..=0x9fff | 0xf900..=0xfaff | 0xff00..=0xffef));
    if cjk { base } else { base + 1.0 }
}

fn bundled_fonts(
    factory: &IDWriteFactory,
    dir: &Path,
) -> windows::core::Result<IDWriteFontCollection> {
    let native: IDWriteFactory5 = factory.cast()?;
    unsafe {
        let builder = native.CreateFontSetBuilder()?;
        for name in ["NotoSans-Regular.ttf", "RobotoMono-Regular.ttf"] {
            let path: Vec<u16> = dir
                .join(name)
                .as_os_str()
                .encode_wide()
                .chain([0])
                .collect();
            if let Ok(font) = factory.CreateFontFileReference(PCWSTR(path.as_ptr()), None) {
                builder.AddFontFile(&font)?;
            }
        }
        native
            .CreateFontCollectionFromFontSet(&builder.CreateFontSet()?)?
            .cast()
    }
}

fn measure(
    factory: Option<&IDWriteFactory>,
    collection: Option<&IDWriteFontCollection>,
    text: &str,
    base: f32,
    font: u8,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let size = text_size(text, base);
    if let Some(factory) = factory {
        let family = match font {
            1 => w!("Noto Sans"),
            2 => w!("Roboto Mono"),
            _ => w!("Arial"),
        };
        let result = unsafe {
            factory
                .CreateTextFormat(
                    family,
                    collection,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("zh-CN"),
                )
                .and_then(|format| {
                    factory.CreateTextLayout(
                        &text.encode_utf16().collect::<Vec<_>>(),
                        &format,
                        65535.0,
                        4096.0,
                    )
                })
                .and_then(|layout| {
                    let mut metrics = DWRITE_TEXT_METRICS::default();
                    layout.GetMetrics(&mut metrics)?;
                    Ok(metrics.widthIncludingTrailingWhitespace)
                })
        };
        if let Ok(width) = result {
            return width;
        }
    }
    text.chars()
        .map(|c| if c.is_ascii() { 0.55 * size } else { size })
        .sum()
}

fn capsule_width(title: f32, subtitle: f32, notice: Option<f32>) -> f32 {
    let width = ((title + 19.0).max(subtitle) + 36.0).clamp(100.0, 300.0);
    width.max(notice.map_or(0.0, |widest| (widest + 55.0).clamp(148.0, 420.0)))
}

#[derive(Clone, Copy)]
struct Scene {
    scale: f32,
    home_x: f32,
    rest_y: f32,
    middle_y: f32,
    half_view: f32,
}

fn project(rect: RECT, height: f32, lift: f32) -> Scene {
    let w = (rect.right - rect.left) as f32;
    let h = (rect.bottom - rect.top) as f32;
    let projection = (w / 2560.0).max(h / 1600.0);
    let scale = projection * 1.5;
    let top_margin = (h / (2.0 * projection) - 670.0).max(46.0);
    let clearance = (top_margin / 1.5 - 23.0).max(0.0);
    let rest_y = rect.top as f32 + projection * top_margin + scale * (lift - clearance).max(0.0);
    Scene {
        scale,
        home_x: rect.left as f32 + w / 2.0,
        rest_y,
        middle_y: rest_y + scale * (height - 36.0 - lift) * 0.5,
        half_view: w / (2.0 * scale),
    }
}

fn dodge_target(scene: Scene, shape: &CapsuleShape, cursor: POINT) -> Option<f32> {
    let x = (cursor.x as f32 - scene.home_x) / scene.scale;
    let y = (cursor.y as f32 - scene.rest_y) / scene.scale;
    let left = shape.satellite.position - 21.0;
    let right = shape.right.position;
    let current = shape.dodge.target;
    let near = y >= -52.0 - shape.lift.position.max(0.0) && y <= shape.height.position + 16.0;
    if near && (left + current - 34.0..=right + current + 34.0).contains(&x) {
        let low = (-scene.half_view + 6.0 - left).clamp(-260.0, 0.0);
        let high = (scene.half_view - 6.0 - right).clamp(0.0, 260.0);
        let bounds = low..=high;
        let to_right = x + 80.0 - left;
        let to_left = x - 80.0 - right;
        let (first, second) = if x < current + (left + right) / 2.0 {
            (to_right, to_left)
        } else {
            (to_left, to_right)
        };
        Some(if bounds.contains(&first) {
            first
        } else if bounds.contains(&second) {
            second
        } else if first.abs() < second.abs() {
            first.clamp(low, high)
        } else {
            second.clamp(low, high)
        })
    } else if near && (left - 34.0..=right + 34.0).contains(&x) {
        Some(current)
    } else {
        None
    }
}

struct Spring {
    position: f32,
    target: f32,
    velocity: f32,
    direction: f32,
}

impl Spring {
    fn new(position: f32) -> Self {
        Self {
            position,
            target: position,
            velocity: 0.0,
            direction: 0.0,
        }
    }
    fn aim(&mut self, target: f32) {
        if self.target != target {
            self.target = target;
            self.direction = (target - self.position).signum();
        }
    }
    fn active(&self) -> bool {
        self.position != self.target || self.velocity != 0.0
    }
    fn step(&mut self, dt: f32, omega: f32, damping: f32, return_damping: f32) {
        let delta = self.target - self.position;
        if self.direction * delta < 0.0 {
            self.direction = 0.0;
        }
        let damping = if self.direction == 0.0 {
            return_damping
        } else {
            damping
        };
        self.velocity += (omega * omega * delta - 2.0 * damping * omega * self.velocity) * dt;
        self.position += self.velocity * dt;
        if delta.abs() < 0.009 && self.velocity.abs() < 0.04 {
            self.position = self.target;
            self.velocity = 0.0;
        }
    }
}

struct CapsuleShape {
    right: Spring,
    left: Spring,
    satellite: Spring,
    height: Spring,
    lift: Spring,
    dodge: Spring,
    left_delay: f32,
    clear_time: f32,
}

impl CapsuleShape {
    fn new(width: f32, height: f32, lift: f32) -> Self {
        Self {
            right: Spring::new(width / 2.0),
            left: Spring::new(-width / 2.0),
            satellite: Spring::new(-width / 2.0),
            height: Spring::new(height),
            lift: Spring::new(lift),
            dodge: Spring::new(0.0),
            left_delay: 0.0,
            clear_time: 0.0,
        }
    }
    fn aim(&mut self, width: f32, height: f32, lift: f32) {
        if self.right.target != width / 2.0 {
            self.right.aim(width / 2.0);
            self.left_delay = 0.070;
        }
        self.height.aim(height);
        self.lift.aim(lift);
    }
    fn advance(&mut self, mut elapsed: f32) {
        while elapsed > 0.0 {
            let dt = elapsed.min(0.004);
            elapsed -= dt;
            if self.left_delay > 0.0 {
                self.left_delay -= dt;
                if self.left_delay <= 0.0 {
                    self.left.aim(-self.right.target);
                }
            }
            self.left.step(dt, 11.0, 1.0, 1.0);
            self.right.step(dt, 11.0, 0.15, 0.85);
            self.satellite.aim(self.left.position);
            self.satellite.step(dt, 22.0, 1.0, 1.0);
            if self.satellite.position > self.left.position {
                self.satellite.position = self.left.position;
                self.satellite.velocity = self.satellite.velocity.min(self.left.velocity);
            }
            for spring in [&mut self.height, &mut self.lift, &mut self.dodge] {
                spring.step(dt, 210.0_f32.sqrt(), 0.72, 0.72);
            }
        }
    }
    fn active(&self) -> bool {
        self.left_delay > 0.0
            || self.right.active()
            || self.left.active()
            || self.satellite.active()
            || self.dodge.active()
            || self.lift.active()
            || self.height.active()
    }
    fn follow_cursor(&mut self, target: Option<f32>, elapsed: f32) -> bool {
        if let Some(target) = target {
            self.dodge.aim(target);
            self.clear_time = 0.0;
        } else {
            self.clear_time += elapsed;
            if self.clear_time >= 0.45 {
                self.dodge.aim(0.0);
            }
        }
        target.is_none() && self.dodge.target != 0.0
    }
}

fn dock_bounds(rect: RECT, dpi: u32, right: f32, middle_y: f32, bottom: f32) -> (RECT, (i32, i32)) {
    let dip = dpi as f32 / 96.0;
    let widget = disc_dpi(dpi) as f32 / 96.0;
    let diameter = 44.0 * widget;
    let gap = 10.0 * dip;
    let max_x = (rect.right as f32 - diameter).floor().max(rect.left as f32) as i32;
    let max_y = (rect.bottom as f32 - diameter).floor().max(rect.top as f32) as i32;
    let right_anchor = (right + gap).ceil() as i32;
    let card_max = (rect.right as f32 - 216.0 * widget).floor() as i32;
    let below = right_anchor > card_max;
    let min_x = if below {
        rect.left
    } else {
        right_anchor.max(rect.left)
    };
    let min_y = if below {
        ((bottom + gap).ceil() as i32).clamp(rect.top, max_y)
    } else {
        rect.top
    };
    let bounds = RECT {
        left: min_x,
        top: min_y,
        right: card_max.clamp(min_x, max_x),
        bottom: max_y,
    };
    let x = if below {
        (right - diameter).round() as i32
    } else {
        right_anchor
    };
    (
        bounds,
        (
            x.clamp(bounds.left, bounds.right),
            ((middle_y - diameter / 2.0).round() as i32).clamp(min_y, max_y),
        ),
    )
}

pub struct Placement {
    monitors: Vec<Monitor>,
    offsets: BTreeMap<String, [f32; 2]>,
    settings_dir: Option<PathBuf>,
    factory: Option<IDWriteFactory>,
    fonts_dir: Option<PathBuf>,
    font_collection: Option<IDWriteFontCollection>,
    cached: Option<(CapsuleData, f32)>,
    shape: CapsuleShape,
    shape_monitor: Option<String>,
    shape_time: Instant,
}

impl Placement {
    pub fn new() -> Self {
        Self::with_dir(data_dir(false))
    }

    fn with_dir(settings_dir: Option<PathBuf>) -> Self {
        let offsets = settings_dir
            .as_deref()
            .map(crate::settings::offsets)
            .unwrap_or_default();
        Self {
            monitors: monitors(),
            offsets,
            settings_dir,
            factory: unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok(),
            fonts_dir: None,
            font_collection: None,
            cached: None,
            shape: CapsuleShape::new(100.0, 36.0, 0.0),
            shape_monitor: None,
            shape_time: Instant::now(),
        }
    }

    pub fn invalidate(&mut self) {
        self.monitors = monitors();
        self.shape_monitor = None;
    }

    pub fn anchor(&mut self, snapshot: &UiSnapshot) -> Anchor {
        let mut cursor = POINT::default();
        let cursor = unsafe { GetCursorPos(&mut cursor) }.ok().map(|_| cursor);
        self.anchor_at(snapshot, Instant::now(), cursor)
    }

    fn anchor_at(&mut self, snapshot: &UiSnapshot, now: Instant, cursor: Option<POINT>) -> Anchor {
        let data = &snapshot.placement;
        let index = data.display.as_deref().and_then(|id| {
            data.paths
                .iter()
                .find_map(|path| monitor_index(&self.monitors, path))
                .or_else(|| monitor_index(&self.monitors, id))
        });
        let monitor = &self.monitors
            [index.unwrap_or_else(|| self.monitors.iter().position(|m| m.primary).unwrap_or(0))];
        let dpi = monitor.dpi;
        let dip = dpi as f32 / 96.0;
        let widget_dip = disc_dpi(dpi) as f32 / 96.0;
        let rect = monitor.rect;
        let mut animating = false;
        let (bounds, base) = if index.is_some() {
            if self.cached.as_ref().is_none_or(|(key, _)| key != data) {
                if data.font != 0 && self.fonts_dir != data.fonts {
                    self.font_collection = self
                        .factory
                        .as_ref()
                        .zip(data.fonts.as_deref())
                        .and_then(|(factory, dir)| bundled_fonts(factory, dir).ok());
                    self.fonts_dir.clone_from(&data.fonts);
                }
                let measure = |text: &str, size| {
                    measure(
                        self.factory.as_ref(),
                        if data.font == 0 {
                            None
                        } else {
                            self.font_collection.as_ref()
                        },
                        text,
                        size,
                        data.font,
                    )
                };
                let notice = data
                    .notice
                    .as_ref()
                    .map(|(label, song)| measure(label, 11.5).max(measure(song, 9.5)));
                let width = if data.unknown_width {
                    300.0_f32.max(notice.map_or(0.0, |_| 420.0))
                } else {
                    capsule_width(
                        measure(&data.title, 13.0),
                        measure(&data.subtitle, 10.0),
                        notice,
                    )
                };
                self.cached = Some((data.clone(), width));
            }
            let width = self.cached.as_ref().unwrap().1;
            let height = if data.subtitle.is_empty() { 36.0 } else { 46.0 };
            let lift = if data.notice.is_some() { 42.0 } else { 0.0 };
            let elapsed = now
                .saturating_duration_since(self.shape_time)
                .as_secs_f32()
                .min(2.0);
            self.shape_time = now;
            if self.shape_monitor.as_ref() != Some(&monitor.key) {
                self.shape = CapsuleShape::new(width, height, lift);
                self.shape_monitor = Some(monitor.key.clone());
            }
            if self.shape.active() {
                self.shape.advance(elapsed);
            }
            self.shape.aim(width, height, lift);
            let scene = project(rect, self.shape.height.position, self.shape.lift.position);
            let target = cursor.and_then(|p| dodge_target(scene, &self.shape, p));
            animating = self.shape.follow_cursor(target, elapsed) || self.shape.active();
            let right = if data.unknown_width {
                let widest = self.shape.right.position.max(self.shape.right.target);
                let rebound = (widest - 50.0).max(0.0) * 0.32;
                widest + rebound
            } else {
                self.shape.right.position
            };
            let edge = scene.home_x + scene.scale * (right + self.shape.dodge.position);
            let bottom = scene.rest_y + scene.scale * (self.shape.height.position - 18.0 + 7.0);
            dock_bounds(rect, dpi, edge, scene.middle_y, bottom)
        } else {
            self.shape_monitor = None;
            let (max_x, max_y) = clamp_position(rect, widget_dip, f32::MAX, f32::MAX);
            let bounds = RECT {
                left: rect.left,
                top: rect.top,
                right: max_x,
                bottom: max_y,
            };
            (
                bounds,
                clamp_position(
                    rect,
                    widget_dip,
                    (rect.left + rect.right) as f32 / 2.0 - 22.0 * widget_dip,
                    rect.top as f32 + 44.0 * dip,
                ),
            )
        };
        let key = monitor.key.clone();
        let [dx, dy] = self.offsets.get(&key).copied().unwrap_or_default();
        let x = ((base.0 as f32 + dx * dip).round() as i32).clamp(bounds.left, bounds.right);
        let y = ((base.1 as f32 + dy * dip).round() as i32).clamp(bounds.top, bounds.bottom);
        Anchor {
            x,
            y,
            dpi,
            hz: monitor.hz,
            key,
            docked: index.is_some(),
            animating,
            base,
            bounds,
        }
    }

    pub fn drag(&mut self, anchor: &Anchor, dx: i32, dy: i32) {
        let dip = anchor.dpi as f32 / 96.0;
        let (x, y) =
            anchor.constrain_drag((anchor.x.saturating_add(dx), anchor.y.saturating_add(dy)));
        self.offsets.insert(
            anchor.key.clone(),
            [
                (x - anchor.base.0) as f32 / dip,
                (y - anchor.base.1) as f32 / dip,
            ],
        );
        self.save(&anchor.key);
    }

    pub fn reset(&mut self, anchor: &Anchor) {
        self.offsets.remove(&anchor.key);
        self.save(&anchor.key);
    }

    fn save(&self, key: &str) {
        if let Some(dir) = &self.settings_dir {
            let _ = crate::settings::set_offset(dir, key, self.offsets.get(key).copied());
        }
    }
}

fn clamp_position(rect: RECT, dip: f32, x: f32, y: f32) -> (i32, i32) {
    (
        x.clamp(
            rect.left as f32,
            (rect.right as f32 - 216.0 * dip).max(rect.left as f32),
        )
        .round() as i32,
        y.clamp(
            rect.top as f32,
            (rect.bottom as f32 - 44.0 * dip).max(rect.top as f32),
        )
        .round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn fallback_drag_and_reset_survive_restarting_on_a_scaled_primary_monitor() {
        let dir = std::env::temp_dir().join(format!(
            "rain-night-offset-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let start = || {
            let mut placement = Placement::with_dir(Some(dir.clone()));
            placement.monitors = vec![Monitor {
                rect: RECT {
                    left: -1920,
                    top: 10,
                    right: 0,
                    bottom: 1090,
                },
                dpi: 144,
                hz: 240.,
                primary: true,
                name: "qa".into(),
                interfaces: vec![],
                key: anonymous_key("qa"),
            }];
            placement
        };
        let view = UiSnapshot::default();
        let mut first = start();
        let base = first.anchor(&view);
        assert_eq!((base.x, base.y), (-987, 10 + 66));
        assert_eq!(base.hz, 240.);
        first.drag(&base, 24, -12);
        let mut second = start();
        let moved = second.anchor(&view);
        assert_eq!((moved.x, moved.y), (base.x + 24, base.y - 12));
        assert_eq!(second.offsets[&moved.key], [16., -8.]);
        second.reset(&moved);
        let reset = start().anchor(&view);
        assert_eq!((reset.x, reset.y), (base.x, base.y));
        fs::remove_file(dir.join("settings.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn two_rows_notice_and_ring_contribute_to_the_real_scene_width() {
        assert_eq!(capsule_width(0., 0., None), 100.);
        assert_eq!(capsule_width(120., 0., None), 175.);
        assert_eq!(capsule_width(50., 180., None), 216.);
        assert_eq!(capsule_width(500., 60., None), 300.);
        assert_eq!(capsule_width(20., 0., Some(500.)), 420.);
        assert_eq!(text_size("测试", 13.), 13.);
        assert_eq!(text_size("Music", 13.), 14.);
    }

    #[test]
    fn projection_matches_cropped_scene_on_mixed_dpi_displays() {
        let rect = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let scene = project(rect, 36.0, 0.0);
        assert_eq!(
            (scene.home_x, scene.middle_y, scene.scale),
            (-960., 37.5, 1.125)
        );
        let two = project(rect, 46.0, 0.0);
        assert_eq!(two.middle_y, 37.5 + 5. * 1.125);
        for dpi in [96, 120, 144] {
            let dip = disc_dpi(dpi) as f32 / 96.;
            assert_eq!(
                clamp_position(rect, dip, 1., 1080.).0,
                (-216. * dip).round() as i32
            );
            assert!((44. * dip - 36. * dpi as f32 / 96.).abs() < 0.5);
        }
        let wide = project(
            RECT {
                left: 0,
                top: 0,
                right: 3840,
                bottom: 1080,
            },
            36.0,
            0.0,
        );
        assert_eq!(wide.rest_y, 69.);
    }

    #[test]
    fn cursor_dodge_stays_within_the_scene_and_releases_away_from_it() {
        let scene = project(
            RECT {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1600,
            },
            36.0,
            0.0,
        );
        let at = |x, y| POINT { x, y };
        let mut shape = CapsuleShape::new(150., 36., 0.);
        assert!(dodge_target(scene, &shape, at(1260, 130)).unwrap() > 0.);
        assert!(dodge_target(scene, &shape, at(1300, 130)).unwrap() < 0.);
        shape.dodge.aim(70.);
        assert_eq!(dodge_target(scene, &shape, at(1300, 450)), None);
        assert!(shape.follow_cursor(None, 0.4));
        assert!(!shape.follow_cursor(None, 0.06));
        assert_eq!(shape.dodge.target, 0.0);
    }

    #[test]
    fn disc_clears_the_scene_spring_peak_with_an_old_negative_drag_offset() {
        for hz in [60, 240] {
            let mut placement = Placement::with_dir(None);
            placement.monitors[0].rect = RECT {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1600,
            };
            placement.monitors[0].dpi = 144;
            let key = placement.monitors[0].key.clone();
            placement.offsets.insert(key, [-60., 0.]);
            let mut view = UiSnapshot::default();
            view.placement.display = Some("Monitor0".into());
            let mut now = Instant::now();
            let frame = Duration::from_nanos(1_000_000_000 / hz);
            let first = placement.anchor_at(&view, now, None);
            assert_eq!(first.x, 1370);
            let long = "很长的测试内容".repeat(16);
            let mut scene_edge = 50.0_f64;
            let mut scene_rate = 0.0_f64;
            let mut previous_x = first.x;
            for (title, subtitle, notice, target) in [
                (long.as_str(), "", false, 150.0_f64),
                ("", "", false, 50.0),
                ("", long.as_str(), false, 150.0),
                ("", long.as_str(), true, 210.0),
                ("", long.as_str(), false, 150.0),
            ] {
                view.placement.title = title.into();
                view.placement.subtitle = subtitle.into();
                view.placement.notice = notice.then(|| ("提示".into(), long.clone()));
                let change = placement.anchor_at(&view, now, None);
                assert_eq!(change.x, previous_x);
                assert!(change.docked && change.animating);
                let mut direction = (target - scene_edge).signum();
                let mut peak = scene_edge;
                for _ in 0..2 * hz {
                    let mut remaining = frame.as_secs_f64();
                    while remaining > 0.0 {
                        let dt = remaining.min(0.004);
                        remaining -= dt;
                        let delta = target - scene_edge;
                        if direction * delta < 0.0 {
                            direction = 0.0;
                        }
                        let damping = if direction == 0.0 { 0.85 } else { 0.15 };
                        scene_rate += (121.0 * delta - 22.0 * damping * scene_rate) * dt;
                        scene_edge += scene_rate * dt;
                        if delta.abs() < 0.009 && scene_rate.abs() < 0.04 {
                            scene_edge = target;
                            scene_rate = 0.0;
                        }
                    }
                    peak = peak.max(scene_edge);
                    now += frame;
                    let a = placement.anchor_at(&view, now, None);
                    let gap = f64::from(a.x) - (1280.0 + 1.5 * scene_edge);
                    assert!((14.999..=16.001).contains(&gap), "{hz} Hz: {gap} px");
                    assert_eq!(a.constrain_drag((0, a.y)).0, a.x);
                    previous_x = a.x;
                }
                if !title.is_empty() {
                    assert!((178.0..181.0).contains(&peak));
                }
                assert_eq!(previous_x, (1280.0 + 1.5 * target + 15.0) as i32);
            }
        }
    }

    #[test]
    fn unknown_width_and_narrow_monitor_keep_the_folded_disc_outside() {
        let mut placement = Placement::with_dir(None);
        let rect = RECT {
            left: -1920,
            top: -160,
            right: 0,
            bottom: 920,
        };
        placement.monitors[0].rect = rect;
        placement.monitors[0].dpi = 96;
        let mut view = UiSnapshot::default();
        view.placement.display = Some("Monitor0".into());
        view.placement.unknown_width = true;
        let first = placement.anchor_at(&view, Instant::now(), None);
        assert_eq!(first.x, (-960.0_f32 + 1.125 * 182.0 + 10.0).ceil() as i32);
        view.placement.notice = Some(("提示".into(), String::new()));
        let notice = placement.anchor_at(&view, Instant::now(), None);
        assert_eq!(notice.x, (-960.0_f32 + 1.125 * 261.2 + 10.0).ceil() as i32);
        let (bounds, base) = dock_bounds(rect, 144, -20.0, 0.0, 46.0);
        assert!(base.1 >= 61 && bounds.top >= 61);
        assert!(base.0 >= rect.left && base.0 + 54 <= rect.right);
        let (bounds, base) = dock_bounds(rect, 144, -120.0, 0.0, 46.0);
        assert!(base.1 >= 61 && bounds.top >= 61);
        let panel = 216.0 * disc_dpi(144) as f32 / 96.0;
        assert!(base.0 as f32 + panel <= rect.right as f32);
    }

    #[test]
    fn directwrite_measures_glyphs_in_scene_units() {
        let factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.unwrap();
        let wide = measure(Some(&factory), None, "WWWW", 13., 0);
        let narrow = measure(Some(&factory), None, "iiii", 13., 0);
        assert!(wide > narrow * 2.);
        assert!(measure(Some(&factory), None, "夜雨", 13., 0) > 15.);
    }

    #[test]
    fn device_interface_paths_and_native_indices_select_the_requested_monitor() {
        let make = |name: &str, id: &str| Monitor {
            rect: RECT::default(),
            dpi: 96,
            hz: 60.,
            primary: false,
            name: name.into(),
            interfaces: vec![id.into()],
            key: anonymous_key(id),
        };
        let monitors = [
            make(r"\\.\DISPLAY4", r"\\?\DISPLAY#demo#first"),
            make(r"\\.\DISPLAY7", r"\\?\DISPLAY#demo#next"),
        ];
        assert_eq!(monitor_index(&monitors, "Monitor1"), Some(1));
        let settings = serde_json::json!({"user":{"monitormap":{
            "//./DISPLAY7":{"location":0,"timestamp":2},"//?/DISPLAY#demo#first":{"location":0,"timestamp":1},"//./DISPLAY4":{"location":1}
        }}});
        let aliases = display_paths(&settings, "Monitor0");
        assert_eq!(aliases[0], "//./DISPLAY7");
        assert_eq!(monitor_index(&monitors, &aliases[0]), Some(1));
        assert_eq!(monitor_index(&monitors, "display#DEMO#FIRST"), Some(0));
        assert_eq!(monitor_index(&monitors, "Display7"), Some(1));
        assert_eq!(monitor_index(&monitors, "Monitor23"), None);
        assert_eq!(monitor_index(&monitors, ""), None);
        assert_ne!(monitors[0].key, monitors[1].key);
    }
}
