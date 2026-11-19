use crate::desktop::{Cover, LookupAnswer};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Foundation::{
    AsyncStatus, EventRegistrationToken, IAsyncOperation, TypedEventHandler,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Playback,
};
use windows::Media::MediaPlaybackType;
use windows::Storage::Streams::IRandomAccessStreamReference;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IStream};
use windows::Win32::System::WinRT::CreateStreamOverRandomAccessStream;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{HRESULT, Result, RuntimeType};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Source {
    NetEase,
    Browser,
    Other,
}

impl Source {
    fn identify(app: &str) -> Self {
        let app = app.to_ascii_lowercase();
        if matches!(
            app.as_str(),
            "cloudmusic.exe" | "1f8b0f94.122165ae053f_j2p0p5q0044a6!app"
        ) {
            Self::NetEase
        } else if matches!(
            app.as_str(),
            "chrome"
                | "chrome.exe"
                | "msedge"
                | "msedge.exe"
                | "firefox"
                | "firefox.exe"
                | "brave"
                | "brave.exe"
                | "vivaldi"
                | "vivaldi.exe"
        ) || app.strip_prefix("chrome.").is_some_and(|suffix| {
            suffix.len() == 26
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        }) {
            Self::Browser
        } else {
            Self::Other
        }
    }

    pub fn from_video(self) -> bool {
        self == Self::Browser
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
    pub fn anonymous(playing: bool) -> Self {
        Self {
            song: Song {
                title: String::new(),
                artist: String::new(),
                source: Source::Other,
            },
            position: None,
            duration: 0.0,
            playing,
            rate: 1.0,
            sent: now_ms(),
        }
    }

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

pub(crate) fn eligible_title(title: &str) -> bool {
    !title.is_empty()
        && title == title.trim()
        && title.chars().count() <= 120
        && !title.chars().any(|c| {
            c.is_control()
                || matches!(c,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' |
            '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
}

pub(crate) fn valid_lookup_input(title: &str, artist: &str) -> bool {
    eligible_title(title) && (artist.is_empty() || eligible_title(artist))
}

fn valid_session_title(title: &str) -> bool {
    !title.trim().is_empty() && title.chars().take(4097).count() <= 4096
}

fn suggested_title(source: Source, original: &str) -> String {
    if source == Source::NetEase && eligible_title(original) {
        original.to_owned()
    } else {
        String::new()
    }
}

fn wallpaper_title(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(320)
        .collect()
}

struct LookupInput {
    title: String,
    artist: String,
}

impl LookupInput {
    fn media(&self, playback: &Media, original_title: &str, local_artist: &str) -> Media {
        let changed_video_title = playback.song.source == Source::Browser
            && !self.title.eq_ignore_ascii_case(original_title.trim());
        let artist = if !self.artist.is_empty() {
            &self.artist
        } else if !changed_video_title && local_artist.chars().count() <= 320 {
            local_artist.trim()
        } else {
            ""
        };
        Media {
            song: Song {
                title: self.title.clone(),
                artist: artist.to_owned(),
                source: playback.song.source,
            },
            duration: if changed_video_title && !self.artist.is_empty() {
                0.0
            } else {
                playback.duration
            },
            position: None,
            rate: 1.0,
            playing: playback.playing,
            sent: playback.sent,
        }
    }
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

#[derive(Clone)]
pub struct LookupPermit {
    current: Arc<AtomicU64>,
    item: Arc<AtomicU64>,
    current_revision: u64,
    item_revision: u64,
}

pub struct ApprovedTrack {
    pub media: Media,
    pub permit: LookupPermit,
    pub id: u64,
    pub lookup_media: Media,
}

impl LookupPermit {
    pub fn valid(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.current_revision
            && self.item.load(Ordering::Acquire) == self.item_revision
    }
}

#[derive(PartialEq)]
struct TrackKey {
    title: String,
    artist: String,
    album: String,
    number: i32,
}

struct TrackConsent {
    id: u64,
    key: Option<TrackKey>,
    source_id: String,
    lookup: Option<LookupInput>,
    offered_at: Instant,
    permit: LookupPermit,
    session: Session,
    token: EventRegistrationToken,
}

fn may_read_item(request: bool, approved: bool, id: u64, reply: Option<u64>) -> bool {
    request || approved || reply == Some(id)
}

impl Drop for TrackConsent {
    fn drop(&mut self) {
        self.permit.item.fetch_add(1, Ordering::AcqRel);
        let _ = self.session.RemoveMediaPropertiesChanged(self.token);
    }
}

pub struct Sessions {
    manager: SessionManager,
    current: Arc<AtomicU64>,
    token: EventRegistrationToken,
    consent: Option<TrackConsent>,
    cover: Option<IRandomAccessStreamReference>,
    pub eligible: bool,
    pub local_playing: Option<bool>,
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.current.fetch_add(1, Ordering::AcqRel);
        let _ = self.manager.RemoveCurrentSessionChanged(self.token);
    }
}

impl Sessions {
    pub fn open() -> Result<Self> {
        let manager = finish(SessionManager::RequestAsync()?)?;
        let current = Arc::new(AtomicU64::new(0));
        let changes = Arc::clone(&current);
        let token = manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            changes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }))?;
        Ok(Self {
            manager,
            current,
            token,
            consent: None,
            cover: None,
            eligible: false,
            local_playing: None,
        })
    }

    pub fn cover(&self, size: u32) -> Option<Cover> {
        let source = self.cover.as_ref()?;
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

    pub fn offer(&self) -> Option<(u64, String)> {
        let consent = self.consent.as_ref()?;
        if consent.lookup.is_some() || !consent.permit.valid() {
            return None;
        }
        Some((
            consent.id,
            suggested_title(
                Source::identify(&consent.source_id),
                &consent.key.as_ref()?.title,
            ),
        ))
    }

    pub fn current(
        &mut self,
        request: bool,
        answer: Option<LookupAnswer>,
    ) -> Result<Option<ApprovedTrack>> {
        static NEXT_OFFER: AtomicU64 = AtomicU64::new(1);
        self.eligible = false;
        self.local_playing = None;
        self.cover = None;
        let current_revision = self.current.load(Ordering::Acquire);
        let session = match self.manager.GetCurrentSession() {
            Ok(s) => s,
            Err(e) if e.code().0 == 0 || e.code().0 == 0x80004003u32 as i32 => {
                self.consent = None;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let app_id = session.SourceAppUserModelId()?.to_string_lossy();
        let source = Source::identify(&app_id);
        let info = session.GetPlaybackInfo()?;
        let status = info.PlaybackStatus()?;
        self.local_playing = match status {
            Playback::Playing => Some(true),
            Playback::Paused => Some(false),
            _ => None,
        };
        let playback_type = info.PlaybackType().and_then(|kind| kind.Value()).ok();
        if source == Source::Other
            || self.local_playing.is_none()
            || (source == Source::NetEase
                && playback_type.is_some_and(|t| t != MediaPlaybackType::Music))
        {
            self.consent = None;
            return Ok(None);
        }
        self.eligible = true;
        if self.consent.as_ref().is_some_and(|c| {
            c.source_id != app_id
                || c.session != session
                || answer
                    .as_ref()
                    .is_some_and(|(id, input)| *id == c.id && input.is_none())
                || !c.permit.valid()
                || (c.lookup.is_none() && c.offered_at.elapsed() > Duration::from_secs(90))
        }) {
            self.consent = None;
        }
        if request {
            self.consent = None;
            let item = Arc::new(AtomicU64::new(0));
            let changes = Arc::clone(&item);
            let token = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                changes.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }))?;
            self.consent = Some(TrackConsent {
                id: NEXT_OFFER.fetch_add(1, Ordering::Relaxed),
                key: None,
                source_id: app_id,
                lookup: None,
                offered_at: Instant::now(),
                permit: LookupPermit {
                    current: Arc::clone(&self.current),
                    current_revision,
                    item,
                    item_revision: 0,
                },
                session: session.clone(),
                token,
            });
        }
        let Some(consent) = self.consent.as_mut() else {
            return Ok(None);
        };
        let submission = answer.and_then(|(id, fields)| fields.map(|fields| (id, fields)));
        if !may_read_item(
            request,
            consent.lookup.is_some(),
            consent.id,
            submission.as_ref().map(|(id, _)| *id),
        ) {
            return Ok(None);
        }
        let metadata = finish(consent.session.TryGetMediaPropertiesAsync()?)?;
        if source == Source::NetEase {
            let kind = metadata.PlaybackType().and_then(|t| t.Value()).ok();
            if kind.or(playback_type) != Some(MediaPlaybackType::Music) {
                self.eligible = false;
                self.consent = None;
                return Ok(None);
            }
        }
        let title = metadata.Title()?.to_string_lossy();
        if !valid_session_title(&title) || !consent.permit.valid() {
            self.consent = None;
            return Ok(None);
        }
        let mut artist = metadata.Artist()?.to_string_lossy();
        if source == Source::NetEase && artist.trim().is_empty() {
            artist = metadata.AlbumArtist().unwrap_or_default().to_string_lossy();
        }
        let key = TrackKey {
            title: title.clone(),
            artist,
            album: metadata.AlbumTitle()?.to_string_lossy(),
            number: metadata.TrackNumber()?,
        };
        if request {
            consent.key = Some(key);
            return Ok(None);
        }
        if consent.key.as_ref() != Some(&key) {
            self.consent = None;
            return Ok(None);
        }
        if let Some((id, (title, artist))) = submission
            && id == consent.id
            && consent.lookup.is_none()
        {
            if !valid_lookup_input(&title, &artist) {
                self.consent = None;
                return Ok(None);
            }
            consent.lookup = Some(LookupInput { title, artist });
        }
        let Some(lookup) = consent.lookup.as_ref() else {
            return Ok(None);
        };
        let permit = consent.permit.clone();
        let id = consent.id;
        self.cover = metadata.Thumbnail().ok();
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
        if !permit.valid() {
            self.consent = None;
            self.cover = None;
            return Ok(None);
        }
        let media = Media {
            song: Song {
                title: wallpaper_title(&title),
                artist: String::new(),
                source,
            },
            duration,
            position,
            playing,
            rate,
            sent,
        };
        let lookup_media = lookup.media(&media, &title, &key.artist);
        Ok(Some(ApprovedTrack {
            media,
            permit,
            id,
            lookup_media,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_source_identity_cannot_be_inferred_from_a_title_or_domain() {
        for unknown in [
            "",
            "A video on bilibili",
            "网易云音乐",
            "https://music.163.com",
            "private-cloudmusic.exe",
            "notchrome.exe",
            "Chrome.NotABrowser",
            "Chrome.abcdefghijklmnopqrstuvwxy!",
            "UnknownApp!CloudMusic",
            "1f8b0f94.122165ae053f_otherpublisher!App",
        ] {
            assert_eq!(Source::identify(unknown), Source::Other);
        }
        assert_eq!(Source::identify("Chrome"), Source::Browser);
        assert_eq!(
            Source::identify("Chrome.ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            Source::Browser
        );
        assert_eq!(Source::identify("MSEdge"), Source::Browser);
        assert_eq!(Source::identify("CloudMusic.exe"), Source::NetEase);
        assert_eq!(
            Source::identify("1f8b0f94.122165ae053f_j2p0p5q0044a6!App"),
            Source::NetEase
        );
    }

    #[test]
    fn consent_requires_a_single_visible_complete_title() {
        assert!(eligible_title("Song / 歌名 🎵"));
        for bad in [
            "",
            "歌名 ",
            "藏\u{202e}字",
            "两行\n内容",
            "字\u{0}字",
            "藏\u{2066}字",
            "字\u{200b}字",
            "字\u{2028}字",
        ] {
            assert!(!eligible_title(bad));
        }
        assert!(!eligible_title(&"雨".repeat(121)));
    }

    #[test]
    fn long_browser_identifiers_never_prefill_a_lookup() {
        let original = format!("私有媒体编号\n{}", "甲".repeat(1000));
        assert!(valid_session_title(&original));
        assert!(valid_session_title(&"雨".repeat(4096)));
        assert!(!valid_session_title(&"雨".repeat(4097)));
        assert!(!valid_lookup_input(&original, ""));
        assert!(!valid_lookup_input("Silver Meadow", "bad\nartist"));
        assert!(valid_lookup_input("Silver Meadow", ""));
        assert_eq!(suggested_title(Source::Browser, "原视频"), "");
        assert_eq!(suggested_title(Source::NetEase, &original), "");
        assert_eq!(
            suggested_title(Source::NetEase, "Silver Meadow"),
            "Silver Meadow"
        );
        assert_eq!(
            wallpaper_title("  本机\n  用来匹配的标题 "),
            "本机 用来匹配的标题"
        );
    }

    #[test]
    fn typed_song_and_performer_do_not_replace_the_local_video_clock_or_title() {
        let mut playing = Media::anonymous(true);
        playing.song.source = Source::Browser;
        playing.song.title = "本地视频".into();
        playing.position = Some(11.7);
        playing.duration = 1800.0;
        let mut input = LookupInput {
            title: "Silver Meadow".into(),
            artist: "Nerai".into(),
        };
        let query = input.media(&playing, "本地视频", "上传者");
        assert_eq!(query.song.title, "Silver Meadow");
        assert_eq!(query.song.artist, "Nerai");
        assert_eq!(query.duration, 0.0);
        assert!(query.position.is_none());
        assert_eq!(playing.song.title, "本地视频");
        assert_eq!(playing.position, Some(11.7));
        assert_eq!(playing.duration, 1800.0);
        assert_eq!(
            input.media(&playing, "Silver Meadow", "宣传标签").duration,
            1800.0
        );
        input.artist.clear();
        let unnamed = input.media(&playing, "本地视频", "上传者");
        assert_eq!(unnamed.song.artist, "");
        assert_eq!(unnamed.duration, 1800.0);
        let named = input.media(&playing, "Silver Meadow", "Nerai");
        assert_eq!(named.song.artist, "Nerai");
        assert_eq!(named.duration, 1800.0);
    }

    #[test]
    fn lookup_requires_a_matching_decision_and_expires_on_both_windows_events() {
        assert!(!may_read_item(false, false, 5, None));
        assert!(!may_read_item(false, false, 5, Some(4)));
        assert!(may_read_item(true, false, 5, None));
        assert!(may_read_item(false, false, 5, Some(5)));
        assert!(may_read_item(false, true, 5, None));
        for current_changed in [true, false] {
            let permit = LookupPermit {
                current: Arc::new(AtomicU64::new(3)),
                item: Arc::new(AtomicU64::new(2)),
                current_revision: 3,
                item_revision: 2,
            };
            let queued = permit.clone();
            assert!(queued.valid());
            if current_changed {
                &permit.current
            } else {
                &permit.item
            }
            .fetch_add(1, Ordering::AcqRel);
            assert!(!queued.valid());
        }
    }
}
