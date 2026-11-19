use crate::desktop::{UiSnapshot, data_dir};
use std::collections::BTreeMap;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
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
}

#[derive(Clone, Debug)]
pub struct Anchor {
    pub x: i32,
    pub y: i32,
    pub dpi: u32,
    pub hz: f64,
    pub key: String,
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

fn project(rect: RECT, rows: bool, notice: bool) -> Scene {
    let w = (rect.right - rect.left) as f32;
    let h = (rect.bottom - rect.top) as f32;
    let projection = (w / 2560.0).max(h / 1600.0);
    let scale = projection * 1.5;
    let top_margin = (h / (2.0 * projection) - 670.0).max(46.0);
    let lift = if notice { 42.0 } else { 0.0 };
    let clearance = (top_margin / 1.5 - 23.0).max(0.0);
    let rest_y = rect.top as f32 + projection * top_margin + scale * (lift - clearance).max(0.0);
    Scene {
        scale,
        home_x: rect.left as f32 + w / 2.0,
        rest_y,
        middle_y: rest_y + scale * ((if rows { 10.0 } else { 0.0 }) - lift) * 0.5,
        half_view: w / (2.0 * scale),
    }
}

fn dodge_target(
    scene: Scene,
    width: f32,
    rows: bool,
    notice: bool,
    current: f32,
    cursor: POINT,
) -> Option<f32> {
    let x = (cursor.x as f32 - scene.home_x) / scene.scale;
    let y = (cursor.y as f32 - scene.rest_y) / scene.scale;
    let left = -width / 2.0 - 21.0;
    let right = width / 2.0;
    let near = y >= -52.0 - if notice { 42.0 } else { 0.0 } && y <= if rows { 62.0 } else { 52.0 };
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

pub struct Placement {
    monitors: Vec<Monitor>,
    offsets: BTreeMap<String, [f32; 2]>,
    settings_dir: Option<PathBuf>,
    factory: Option<IDWriteFactory>,
    fonts_dir: Option<PathBuf>,
    font_collection: Option<IDWriteFontCollection>,
    cached: Option<(CapsuleData, f32)>,
    dodge: f32,
    last_near: Option<Instant>,
}

impl Placement {
    pub fn new() -> Self {
        Self::with_dir(data_dir(false))
    }

    fn with_dir(settings_dir: Option<PathBuf>) -> Self {
        let offsets = settings_dir
            .as_ref()
            .and_then(|dir| fs::read(dir.join("settings.json")).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            monitors: monitors(),
            offsets,
            settings_dir,
            factory: unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok(),
            fonts_dir: None,
            font_collection: None,
            cached: None,
            dodge: 0.0,
            last_near: None,
        }
    }

    pub fn invalidate(&mut self) {
        self.monitors = monitors();
        self.dodge = 0.0;
        self.last_near = None;
    }

    pub fn anchor(&mut self, snapshot: &UiSnapshot) -> Anchor {
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
        let (x, y) = if index.is_some() {
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
                let width = capsule_width(
                    measure(&data.title, 13.0),
                    measure(&data.subtitle, 10.0),
                    notice,
                );
                self.cached = Some((data.clone(), width));
            }
            let width = self.cached.as_ref().unwrap().1;
            let rows = !data.subtitle.is_empty();
            let notice = data.notice.is_some();
            let scene = project(rect, rows, notice);
            let now = Instant::now();
            let mut cursor = POINT::default();
            let target = unsafe { GetCursorPos(&mut cursor) }
                .ok()
                .and_then(|_| dodge_target(scene, width, rows, notice, self.dodge, cursor));
            if let Some(target) = target {
                self.dodge = target;
                self.last_near = Some(now);
            } else if self
                .last_near
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(450))
            {
                self.dodge = 0.0;
            }
            (
                scene.home_x + scene.scale * (width / 2.0 + self.dodge) + 10.0 * dip,
                scene.middle_y - 22.0 * widget_dip,
            )
        } else {
            self.dodge = 0.0;
            self.last_near = None;
            (
                (rect.left + rect.right) as f32 / 2.0 - 22.0 * widget_dip,
                rect.top as f32 + 44.0 * dip,
            )
        };
        let key = monitor.key.clone();
        let [dx, dy] = self.offsets.get(&key).copied().unwrap_or_default();
        let (x, y) = clamp_position(rect, widget_dip, x + dx * dip, y + dy * dip);
        Anchor {
            x,
            y,
            dpi,
            hz: monitor.hz,
            key,
        }
    }

    pub fn drag(&mut self, anchor: &Anchor, dx: i32, dy: i32) {
        let dip = anchor.dpi as f32 / 96.0;
        let (x, y) = (anchor.x.saturating_add(dx), anchor.y.saturating_add(dy));
        let (x, y) = self
            .monitors
            .iter()
            .find(|m| m.key == anchor.key)
            .map_or((x, y), |m| {
                clamp_position(
                    m.rect,
                    disc_dpi(anchor.dpi) as f32 / 96.0,
                    x as f32,
                    y as f32,
                )
            });
        let offset = self.offsets.entry(anchor.key.clone()).or_default();
        offset[0] += (x - anchor.x) as f32 / dip;
        offset[1] += (y - anchor.y) as f32 / dip;
        self.save();
    }

    pub fn reset(&mut self, anchor: &Anchor) {
        self.offsets.remove(&anchor.key);
        self.save();
    }

    fn save(&self) {
        let (Some(dir), Ok(bytes)) = (&self.settings_dir, serde_json::to_vec(&self.offsets)) else {
            return;
        };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let temp = dir.join("settings.tmp");
        if fs::write(&temp, bytes).is_ok() {
            let _ = fs::rename(temp, dir.join("settings.json"));
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
        let scene = project(rect, false, false);
        assert_eq!(
            (scene.home_x, scene.middle_y, scene.scale),
            (-960., 37.5, 1.125)
        );
        let two = project(rect, true, false);
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
            false,
            false,
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
            false,
            false,
        );
        let at = |x, y| POINT { x, y };
        assert!(dodge_target(scene, 150., false, false, 0., at(1260, 130)).unwrap() > 0.);
        assert!(dodge_target(scene, 150., false, false, 0., at(1300, 130)).unwrap() < 0.);
        assert_eq!(
            dodge_target(scene, 150., false, false, 70., at(1300, 450)),
            None
        );
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
