use crate::desktop::Cover;
use std::cell::RefCell;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Foundation::{AsyncStatus, IAsyncOperation};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Playback,
};
use windows::Storage::Streams::IRandomAccessStreamReference;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IStream};
use windows::Win32::System::WinRT::CreateStreamOverRandomAccessStream;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{HRESULT, Result, RuntimeType};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Source {
    Bilibili,
    NetEase,
    Browser,
    Other,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bilibili => "B 站",
            Self::NetEase => "网易云音乐",
            Self::Browser => "浏览器",
            Self::Other => "媒体播放器",
        }
    }

    fn identify(app: &str, title: &str, album: &str) -> Self {
        let app = app.to_lowercase();
        let site = format!("{title} {album}").to_lowercase();
        if app.contains("cloudmusic") || app.contains("netease") || site.contains("网易云音乐")
        {
            Self::NetEase
        } else if app.contains("bilibili") || site.contains("bilibili") || site.contains("哔哩哔哩")
        {
            Self::Bilibili
        } else if ["chrome", "msedge", "firefox", "brave", "browser", "vivaldi"]
            .iter()
            .any(|word| app.contains(word))
        {
            Self::Browser
        } else {
            Self::Other
        }
    }

    pub fn from_video(self) -> bool {
        matches!(self, Self::Bilibili | Self::Browser)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Song {
    pub title: String,
    pub artist: String,
    pub source: Source,
}

#[derive(Clone, Debug)]
pub struct Media {
    pub song: Song,
    pub position: Option<f64>,
    pub duration: f64,
    pub playing: bool,
    pub rate: f64,
    pub sent: i64,
}

impl Media {
    pub fn needs_sync(&self, previous: &Self) -> bool {
        let seconds = (self.sent - previous.sent) as f64 / 1000.0;
        if self.song != previous.song
            || self.playing != previous.playing
            || self.rate != previous.rate
            || (self.duration - previous.duration).abs() > 1.0
            || !(0.0..5.0).contains(&seconds)
        {
            return true;
        }
        match (self.position, previous.position) {
            (Some(now), Some(old)) => {
                let mut expected = old
                    + if previous.playing {
                        seconds * previous.rate
                    } else {
                        0.0
                    };
                if previous.duration > 0.0 {
                    expected = expected.min(previous.duration);
                }
                (now - expected).abs() > 0.9
            }
            (None, None) => false,
            _ => true,
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(320)
        .collect()
}

fn finish<T: RuntimeType + 'static>(op: IAsyncOperation<T>) -> Result<T> {
    let until = Instant::now() + Duration::from_secs(3);
    while op.Status()? == AsyncStatus::Started {
        if Instant::now() >= until {
            let _ = op.Cancel();
            return Err(HRESULT(0x800705B4u32 as i32).into());
        }
        thread::sleep(Duration::from_millis(12));
    }
    op.GetResults()
}

pub struct Apartment;
impl Apartment {
    pub fn open() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() }
    }
}

pub struct Sessions(
    SessionManager,
    RefCell<Option<IRandomAccessStreamReference>>,
);
impl Sessions {
    pub fn open() -> Result<Self> {
        Ok(Self(
            finish(SessionManager::RequestAsync()?)?,
            RefCell::default(),
        ))
    }

    pub fn cover(&self, size: u32) -> Option<Cover> {
        let source = self.1.borrow().clone()?;
        let stream = finish(source.OpenReadAsync().ok()?).ok()?;
        if stream.Size().ok()? > 32 * 1024 * 1024 {
            return None;
        }
        unsafe {
            let input: IStream = CreateStreamOverRandomAccessStream(&stream).ok()?;
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let decoder = factory
                .CreateDecoderFromStream(&input, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
                .ok()?;
            let frame = decoder.GetFrame(0).ok()?;
            let (mut width, mut height) = (0, 0);
            frame.GetSize(&mut width, &mut height).ok()?;
            let side = width.min(height);
            if side == 0
                || width.max(height) > 16384
                || u64::from(width) * u64::from(height) > 64_000_000
            {
                return None;
            }
            let crop = factory.CreateBitmapClipper().ok()?;
            crop.Initialize(
                &frame,
                &WICRect {
                    X: ((width - side) / 2) as i32,
                    Y: ((height - side) / 2) as i32,
                    Width: side as i32,
                    Height: side as i32,
                },
            )
            .ok()?;
            let scaler = factory.CreateBitmapScaler().ok()?;
            scaler
                .Initialize(&crop, size, size, WICBitmapInterpolationModeFant)
                .ok()?;
            let converter = factory.CreateFormatConverter().ok()?;
            converter
                .Initialize(
                    &scaler,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )
                .ok()?;
            let mut pixels = vec![0; (size * size * 4) as usize];
            converter
                .CopyPixels(std::ptr::null(), size * 4, &mut pixels)
                .ok()?;
            Some(Cover { size, pixels })
        }
    }

    pub fn current(&self) -> Result<Option<Media>> {
        let session = match self.0.GetCurrentSession() {
            Ok(s) => s,
            Err(e) if e.code().0 == 0 || e.code().0 == 0x80004003u32 as i32 => return Ok(None),
            Err(e) => return Err(e),
        };
        let info = session.GetPlaybackInfo()?;
        let status = info.PlaybackStatus()?;
        if status != Playback::Playing && status != Playback::Paused {
            return Ok(None);
        }
        let metadata = finish(session.TryGetMediaPropertiesAsync()?)?;
        self.1.replace(metadata.Thumbnail().ok());
        let title = text(&metadata.Title()?.to_string_lossy());
        if title.is_empty() {
            return Ok(None);
        }
        let mut artist = text(&metadata.Artist().unwrap_or_default().to_string_lossy());
        if artist.is_empty() {
            artist = text(&metadata.AlbumArtist().unwrap_or_default().to_string_lossy());
        }
        let app = session
            .SourceAppUserModelId()
            .unwrap_or_default()
            .to_string_lossy();
        let album = metadata.AlbumTitle().unwrap_or_default().to_string_lossy();
        let source = Source::identify(&app, &title, &album);
        let rate = info
            .PlaybackRate()
            .and_then(|value| value.Value())
            .ok()
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(1.0);
        let playing = status == Playback::Playing;
        let sent = now_ms();
        let mut duration = 0.0;
        let mut position = None;
        if let Ok(clock) = session.GetTimelineProperties() {
            let start = clock.StartTime().map(|t| t.Duration).unwrap_or(0);
            if let Ok(end) = clock.EndTime() {
                duration = ((end.Duration.saturating_sub(start)) as f64 / 10_000_000.0).max(0.0);
            }
            if let Ok(sample) = clock.Position() {
                if duration > 0.0 || sample.Duration > start {
                    let mut p =
                        (sample.Duration.saturating_sub(start) as f64 / 10_000_000.0).max(0.0);
                    if let (true, Ok(stamp)) = (playing, clock.LastUpdatedTime()) {
                        let unix_ms = stamp.UniversalTime / 10_000 - 11_644_473_600_000;
                        let elapsed = (sent - unix_ms) as f64 / 1000.0;
                        if (0.0..86_400.0).contains(&elapsed) {
                            p += elapsed * rate;
                        }
                    }
                    position = Some(if duration > 0.0 { p.min(duration) } else { p });
                }
            }
        }
        Ok(Some(Media {
            song: Song {
                title,
                artist,
                source,
            },
            duration,
            position,
            playing,
            rate,
            sent,
        }))
    }
}
