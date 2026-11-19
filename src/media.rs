use crate::desktop::{Cover, LookupAnswer};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::ApplicationModel::AppInfo;
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
    MusicApp,
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
            Self::MusicApp
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
        self != Self::MusicApp
    }

    fn with_registered_name(self, name: &str) -> Self {
        if self == Self::Other
            && (name == "哔哩哔哩" || name == "嗶哩嗶哩" || name.eq_ignore_ascii_case("bilibili"))
        {
            Self::Browser
        } else {
            self
        }
    }

    fn for_playback(
        self,
        info: Option<MediaPlaybackType>,
        item: Option<MediaPlaybackType>,
    ) -> Self {
        if self != Self::MusicApp {
            return self;
        }
        if info.or(item).is_none()
            || [info, item]
                .into_iter()
                .flatten()
                .any(|t| t != MediaPlaybackType::Music)
        {
            Self::Browser
        } else {
            Self::MusicApp
        }
    }

    fn lookup_mode(self, auto: Option<bool>) -> LookupMode {
        match (self, auto) {
            (Self::MusicApp, None) => LookupMode::AwaitChoice,
            (Self::MusicApp, Some(true)) => LookupMode::Automatic,
            _ => LookupMode::Manual,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LookupMode {
    AwaitChoice,
    Automatic,
    Manual,
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
        let changed_video_title = playback.song.source.from_video()
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
    pub display_artist: String,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackKey {
    title: String,
    artist: String,
    album: String,
    number: i32,
}

struct ActiveTrack {
    id: u64,
    key: TrackKey,
    source: Source,
    automatic: bool,
    permit: LookupPermit,
}

#[derive(Debug, PartialEq, Eq)]
enum TrackChange {
    Same,
    Automatic,
    Offer,
    Inactive,
}

impl ActiveTrack {
    fn new(
        key: TrackKey,
        source: Source,
        automatic: bool,
        current: &Arc<AtomicU64>,
        revision: u64,
    ) -> Self {
        static NEXT_TRACK: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_TRACK.fetch_add(1, Ordering::Relaxed),
            key,
            source,
            automatic,
            permit: LookupPermit {
                current: Arc::clone(current),
                current_revision: revision,
                item: Arc::new(AtomicU64::new(0)),
                item_revision: 0,
            },
        }
    }

    fn matches(&self, key: &TrackKey, source: Source, automatic: bool) -> bool {
        self.source == source
            && self.automatic == automatic
            && self.key == *key
            && self.permit.valid()
    }

    fn observe(
        slot: &mut Option<Self>,
        key: TrackKey,
        source: Source,
        mode: LookupMode,
        request: bool,
        current: &Arc<AtomicU64>,
        revision: u64,
    ) -> TrackChange {
        let automatic = mode == LookupMode::Automatic;
        let offer = mode == LookupMode::Manual && request;
        if !offer
            && mode != LookupMode::AwaitChoice
            && slot
                .as_ref()
                .is_some_and(|track| track.matches(&key, source, automatic))
        {
            return TrackChange::Same;
        }
        *slot = None;
        if !automatic && !offer {
            return TrackChange::Inactive;
        }
        *slot = Some(Self::new(key, source, automatic, current, revision));
        if offer {
            TrackChange::Offer
        } else {
            TrackChange::Automatic
        }
    }
}

impl Drop for ActiveTrack {
    fn drop(&mut self) {
        self.permit.item.fetch_add(1, Ordering::AcqRel);
    }
}

struct TrackConsent {
    lookup: Option<LookupInput>,
    offered_at: Instant,
}

struct LayoutEvents {
    session: Session,
    token: EventRegistrationToken,
    revision: Arc<AtomicU64>,
    seen: u64,
}

impl LayoutEvents {
    fn listen(session: &Session) -> Result<Self> {
        let revision = Arc::new(AtomicU64::new(1));
        let signal = Arc::clone(&revision);
        let owner = thread::current();
        let token = session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
            signal.fetch_add(1, Ordering::AcqRel);
            owner.unpark();
            Ok(())
        }))?;
        Ok(Self {
            session: session.clone(),
            token,
            revision,
            seen: 0,
        })
    }
}

impl Drop for LayoutEvents {
    fn drop(&mut self) {
        let _ = self.session.RemoveMediaPropertiesChanged(self.token);
    }
}

pub struct Sessions {
    manager: SessionManager,
    current: Arc<AtomicU64>,
    token: EventRegistrationToken,
    track: Option<ActiveTrack>,
    consent: Option<TrackConsent>,
    deferred_action: Option<(bool, Option<LookupAnswer>)>,
    layout_events: Option<LayoutEvents>,
    pub notice_deadline: Option<Instant>,
    cover: Option<IRandomAccessStreamReference>,
    pub eligible: bool,
    pub local_playing: Option<bool>,
    pub source: Option<Source>,
    pub app_id: Option<String>,
    app_source: Source,
    pub rechecking: bool,
    pub cover_changed: bool,
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
        let owner = thread::current();
        let token = manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            changes.fetch_add(1, Ordering::AcqRel);
            owner.unpark();
            Ok(())
        }))?;
        Ok(Self {
            manager,
            current,
            token,
            track: None,
            consent: None,
            deferred_action: None,
            layout_events: None,
            notice_deadline: None,
            cover: None,
            eligible: false,
            local_playing: None,
            source: None,
            app_id: None,
            app_source: Source::Other,
            rechecking: false,
            cover_changed: false,
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
        let track = self.track.as_ref()?;
        if consent.lookup.is_some() || !track.permit.valid() {
            return None;
        }
        Some((
            track.id,
            crate::lookup::suggested_title(&track.key.title, track.source),
        ))
    }

    pub fn revoke(&mut self) {
        self.track = None;
        self.consent = None;
        self.deferred_action = None;
        self.cover = None;
    }

    pub fn needs_refresh(&self) -> bool {
        self.layout_events
            .as_ref()
            .is_some_and(|watch| watch.revision.load(Ordering::Acquire) != watch.seen)
    }

    pub fn current(
        &mut self,
        request: bool,
        answer: Option<LookupAnswer>,
        auto: Option<bool>,
    ) -> Result<Option<ApprovedTrack>> {
        let (previous_request, previous_answer) = self.deferred_action.take().unwrap_or_default();
        let request = request || previous_request;
        let answer = answer.or(previous_answer);
        self.eligible = false;
        self.local_playing = None;
        self.source = None;
        self.rechecking = false;
        self.cover_changed = false;
        let current_revision = self.current.load(Ordering::Acquire);
        let session = match self.manager.GetCurrentSession() {
            Ok(s) => s,
            Err(e) if e.code().0 == 0 || e.code().0 == 0x80004003u32 as i32 => {
                self.revoke();
                self.layout_events = None;
                self.notice_deadline = None;
                self.app_id = None;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if self
            .layout_events
            .as_ref()
            .is_some_and(|events| events.session != session)
        {
            self.revoke();
            self.layout_events = None;
        }
        if self.layout_events.is_none() {
            self.layout_events = LayoutEvents::listen(&session).ok();
        }
        if let Some(events) = &mut self.layout_events {
            let observed = events.revision.load(Ordering::Acquire);
            self.cover_changed = observed != events.seen;
            events.seen = observed;
        }
        let app_id = session.SourceAppUserModelId()?.to_string_lossy();
        if self.app_id.as_deref() != Some(&app_id) {
            self.app_source = Source::identify(&app_id);
            if self.app_source == Source::Other
                && let Ok(name) = AppInfo::GetFromAppUserModelId(&app_id.as_str().into())
                    .and_then(|info| info.DisplayInfo())
                    .and_then(|display| display.DisplayName())
            {
                self.app_source = self
                    .app_source
                    .with_registered_name(&name.to_string_lossy());
            }
            self.app_id = Some(app_id);
        }
        let app = self.app_source;
        let info = session.GetPlaybackInfo()?;
        let status = info.PlaybackStatus()?;
        self.local_playing = match status {
            Playback::Playing => Some(true),
            Playback::Paused => Some(false),
            _ => None,
        };
        let info_type = info.PlaybackType().and_then(|kind| kind.Value()).ok();
        if self.local_playing.is_none() {
            self.source = Some(app.for_playback(info_type, None));
            self.revoke();
            return Ok(None);
        }
        let properties = if app == Source::MusicApp
            && info_type.is_none_or(|kind| kind == MediaPlaybackType::Music)
        {
            Some(finish(session.TryGetMediaPropertiesAsync()?)?)
        } else {
            None
        };
        let item_type = properties
            .as_ref()
            .and_then(|item| item.PlaybackType().and_then(|kind| kind.Value()).ok());
        let source = app.for_playback(info_type, item_type);
        self.source = Some(source);
        let mode = source.lookup_mode(auto);
        self.eligible = mode == LookupMode::Manual;
        if mode == LookupMode::AwaitChoice {
            self.revoke();
            return Ok(None);
        }
        let automatic = mode == LookupMode::Automatic;
        let cancelled = answer.as_ref().is_some_and(|(id, input)| {
            input.is_none() && self.track.as_ref().is_some_and(|track| track.id == *id)
        });
        if cancelled
            || self.track.as_ref().is_some_and(|track| {
                !track.permit.valid() || track.automatic != automatic || track.source != source
            })
            || self.consent.as_ref().is_some_and(|consent| {
                consent.lookup.is_none() && consent.offered_at.elapsed() > Duration::from_secs(90)
            })
        {
            self.revoke();
        }
        if !automatic && !request && self.consent.is_none() {
            if self.cover_changed {
                self.notice_deadline = Some(Instant::now() + Duration::from_millis(3450));
            }
            return Ok(None);
        }
        let metadata = match properties {
            Some(item) => item,
            None => finish(session.TryGetMediaPropertiesAsync()?)?,
        };
        let title = metadata.Title()?.to_string_lossy();
        if !valid_session_title(&title) {
            self.revoke();
            return Ok(None);
        }
        let mut artist = metadata.Artist()?.to_string_lossy();
        if source == Source::MusicApp && artist.trim().is_empty() {
            artist = metadata.AlbumArtist().unwrap_or_default().to_string_lossy();
        }
        let key = TrackKey {
            title,
            artist,
            album: metadata.AlbumTitle()?.to_string_lossy(),
            number: metadata.TrackNumber()?,
        };
        if self.current.load(Ordering::Acquire) != current_revision {
            self.revoke();
            self.rechecking = true;
            return Ok(None);
        }
        if self.needs_refresh() {
            self.deferred_action = Some((request, answer));
            self.rechecking = true;
            return Ok(None);
        }
        if self.track.as_ref().is_some_and(|track| track.key != key) {
            self.notice_deadline = Some(Instant::now() + Duration::from_millis(3450));
        }
        match ActiveTrack::observe(
            &mut self.track,
            key,
            source,
            mode,
            request,
            &self.current,
            current_revision,
        ) {
            TrackChange::Same => {}
            TrackChange::Automatic => self.consent = None,
            TrackChange::Inactive => {
                self.revoke();
                return Ok(None);
            }
            TrackChange::Offer => {
                self.cover = None;
                self.consent = Some(TrackConsent {
                    lookup: None,
                    offered_at: Instant::now(),
                });
                return Ok(None);
            }
        }
        let track = self.track.as_ref().unwrap();
        let key = &track.key;
        let automatic_input;
        let lookup = if automatic {
            let title = key.title.trim();
            if !eligible_title(title) {
                return Ok(None);
            }
            automatic_input = LookupInput {
                title: title.to_owned(),
                artist: String::new(),
            };
            &automatic_input
        } else {
            let Some(consent) = self.consent.as_mut() else {
                return Ok(None);
            };
            if let Some((id, Some((title, artist)))) = answer
                && id == track.id
                && consent.lookup.is_none()
            {
                if !valid_lookup_input(&title, &artist) {
                    self.revoke();
                    return Ok(None);
                }
                consent.lookup = Some(LookupInput { title, artist });
            }
            let Some(lookup) = &consent.lookup else {
                return Ok(None);
            };
            lookup
        };
        let permit = track.permit.clone();
        let id = track.id;
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
            self.revoke();
            self.rechecking = true;
            return Ok(None);
        }
        let media = Media {
            song: Song {
                title: wallpaper_title(&key.title),
                artist: String::new(),
                source,
            },
            duration,
            position,
            playing,
            rate,
            sent,
        };
        let lookup_media = lookup.media(&media, &key.title, &key.artist);
        let display_artist = if key.artist.trim().is_empty() {
            metadata.AlbumArtist().unwrap_or_default().to_string_lossy()
        } else {
            key.artist.clone()
        };
        Ok(Some(ApprovedTrack {
            media,
            display_artist: wallpaper_title(&display_artist),
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
        assert_eq!(Source::identify("CloudMusic.exe"), Source::MusicApp);
        assert_eq!(
            Source::identify("1f8b0f94.122165ae053f_j2p0p5q0044a6!App"),
            Source::MusicApp
        );
    }

    #[test]
    fn registered_application_names_can_only_identify_a_manual_video_source() {
        for name in ["哔哩哔哩", "嗶哩嗶哩", "BiliBili"] {
            assert_eq!(Source::Other.with_registered_name(name), Source::Browser);
        }
        for name in [
            "",
            "Bilibili 插件",
            "音乐 - bilibili",
            "网易云音乐",
            "CloudMusic.exe",
        ] {
            assert_eq!(Source::Other.with_registered_name(name), Source::Other);
        }
        assert_eq!(
            Source::MusicApp.with_registered_name("Bilibili"),
            Source::MusicApp
        );
    }

    #[test]
    fn only_explicit_music_from_the_allowlist_can_use_automatic_lookup() {
        use MediaPlaybackType as Kind;
        for (info, item, expected) in [
            (None, None, Source::Browser),
            (Some(Kind::Music), None, Source::MusicApp),
            (None, Some(Kind::Music), Source::MusicApp),
            (Some(Kind::Music), Some(Kind::Video), Source::Browser),
            (Some(Kind::Video), Some(Kind::Music), Source::Browser),
            (None, Some(Kind::Unknown), Source::Browser),
        ] {
            assert_eq!(Source::MusicApp.for_playback(info, item), expected);
            assert_eq!(Source::Browser.for_playback(info, item), Source::Browser);
            assert_eq!(Source::Other.for_playback(info, item), Source::Other);
        }
        assert_eq!(Source::MusicApp.lookup_mode(None), LookupMode::AwaitChoice);
        assert_eq!(
            Source::MusicApp.lookup_mode(Some(false)),
            LookupMode::Manual
        );
        assert_eq!(
            Source::MusicApp.lookup_mode(Some(true)),
            LookupMode::Automatic
        );
        for choice in [None, Some(false), Some(true)] {
            for source in [Source::Browser, Source::Other] {
                assert_eq!(source.lookup_mode(choice), LookupMode::Manual);
            }
        }
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
    fn manual_prefill_is_a_visible_editable_song_candidate() {
        let original = format!("私有媒体编号\n{}", "甲".repeat(1000));
        assert!(valid_session_title(&original));
        assert!(valid_session_title(&"雨".repeat(4096)));
        assert!(!valid_session_title(&"雨".repeat(4097)));
        assert!(!valid_lookup_input(&original, ""));
        assert!(!valid_lookup_input("Silver Meadow", "bad\nartist"));
        assert!(valid_lookup_input("Silver Meadow", ""));
        use crate::lookup::suggested_title;
        assert_eq!(suggested_title("原视频", Source::Browser), "原视频");
        for source in [Source::Browser, Source::Other] {
            assert_eq!(
                suggested_title("【高音质】演奏《窗前雨》 | 哔哩哔哩", source),
                "窗前雨"
            );
            assert_eq!(
                suggested_title("歌手 - Silver Meadow | bilibili", source),
                "Silver Meadow"
            );
        }
        assert_eq!(suggested_title(&original, Source::MusicApp), "");
        assert_eq!(
            suggested_title("Silver Meadow", Source::MusicApp),
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
    fn track_identity_survives_seek_or_late_art_and_rejects_changed_fields() {
        let key = TrackKey {
            title: "曲".into(),
            artist: "歌手".into(),
            album: "专辑".into(),
            number: 4,
        };
        let session_revision = Arc::new(AtomicU64::new(3));
        let track = ActiveTrack::new(key.clone(), Source::Browser, false, &session_revision, 3);
        let queued = track.permit.clone();
        assert!(track.matches(&key, Source::Browser, false));
        for changed in [
            TrackKey {
                title: "新曲".into(),
                ..key.clone()
            },
            TrackKey {
                artist: "另一位".into(),
                ..key.clone()
            },
            TrackKey {
                album: "另一版".into(),
                ..key.clone()
            },
            TrackKey {
                number: 5,
                ..key.clone()
            },
        ] {
            assert!(!track.matches(&changed, Source::Browser, false));
        }
        assert!(!track.matches(&key, Source::MusicApp, true));
        assert!(queued.valid());
        drop(track);
        assert!(!queued.valid());
    }

    #[test]
    #[ignore = "explicit local SMTC browser acceptance using RAIN_NIGHT_VERIFY_BROWSER=1"]
    fn live_browser_keeps_automatic_lookup_off_and_prefills_manual_offer() {
        if std::env::var_os("RAIN_NIGHT_VERIFY_BROWSER").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        let _apartment = Apartment::open().expect("Windows Runtime unavailable");
        let mut sessions = Sessions::open().expect("Windows media control unavailable");
        let approved = sessions
            .current(false, None, Some(true))
            .expect("session inspection failed");
        assert!(
            approved.is_none() && sessions.offer().is_none(),
            "automatic lookup was offered"
        );
        assert!(
            sessions.source == Some(Source::Browser) && sessions.eligible,
            "an active browser media session is required"
        );
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut request = true;
        loop {
            let approved = sessions
                .current(request, None, Some(true))
                .expect("manual offer failed");
            request = false;
            assert!(
                approved.is_none(),
                "manual request granted without confirmation"
            );
            if let Some((_, candidate)) = sessions.offer() {
                let nonempty = !candidate.is_empty();
                println!("browser_auto_disabled=true; manual_candidate_nonempty={nonempty}");
                assert!(
                    nonempty && eligible_title(&candidate),
                    "no eligible song candidate"
                );
                break;
            }
            assert!(
                sessions.rechecking && Instant::now() < deadline,
                "no current manual offer"
            );
            thread::sleep(Duration::from_millis(20));
        }
        sessions.revoke();
    }

    #[test]
    #[ignore = "explicit live browser song switch with RAIN_NIGHT_VERIFY_BROWSER_SWITCH=1"]
    fn live_browser_change_expires_previous_manual_lookup() {
        if std::env::var_os("RAIN_NIGHT_VERIFY_BROWSER_SWITCH").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        fn read(
            sessions: &mut Sessions,
            request: bool,
            answer: Option<LookupAnswer>,
        ) -> Option<ApprovedTrack> {
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut result = sessions
                .current(request, answer, Some(true))
                .expect("browser inspection failed");
            while sessions.rechecking {
                assert!(Instant::now() < deadline, "unstable browser media session");
                thread::sleep(Duration::from_millis(25));
                result = sessions
                    .current(false, None, Some(true))
                    .expect("browser recheck failed");
            }
            result
        }
        let _apartment = Apartment::open().expect("Windows Runtime unavailable");
        let mut sessions = Sessions::open().expect("Windows media control unavailable");
        assert!(read(&mut sessions, false, None).is_none());
        assert!(
            sessions.source == Some(Source::Browser) && sessions.eligible,
            "active browser required"
        );
        let original_app = sessions.app_id.clone();
        assert!(read(&mut sessions, true, None).is_none());
        let (mut id, _) = sessions.offer().expect("manual offer required");
        let key = sessions
            .track
            .as_ref()
            .expect("missing local track identity")
            .key
            .clone();
        let answer = |id| Some((id, Some(("离线验收".to_owned(), String::new()))));
        let mut first =
            read(&mut sessions, false, answer(id)).expect("initial manual approval failed");
        println!("browser_switch_probe_armed=true");
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            thread::park_timeout(Duration::from_millis(180));
            if let Some(active) = read(&mut sessions, false, None) {
                assert!(
                    active.id == id && first.permit.valid(),
                    "unexpected automatic grant"
                );
                continue;
            }
            if sessions.source != Some(Source::Browser)
                || sessions.app_id != original_app
                || !sessions.eligible
            {
                continue;
            }
            assert!(!first.permit.valid() && sessions.offer().is_none());
            assert!(
                read(&mut sessions, false, answer(id)).is_none(),
                "old answer granted access"
            );
            assert!(
                read(&mut sessions, true, None).is_none(),
                "manual request granted before confirmation"
            );
            let Some((new_id, _)) = sessions.offer() else {
                continue;
            };
            assert!(id != new_id);
            if sessions
                .track
                .as_ref()
                .is_some_and(|track| track.key != key)
            {
                println!(
                    "browser_track_changed=true; old_grant_revoked=true; new_confirmation_required=true"
                );
                return;
            }
            id = new_id;
            first = read(&mut sessions, false, answer(id)).expect("same-item recheck failed");
        }
        panic!("no distinct active browser track observed in two minutes");
    }

    #[test]
    fn five_tracks_switch_directly_in_automatic_mode_and_revoke_manual_offers() {
        let generation = Arc::new(AtomicU64::new(7));
        for (source, choice, expected) in [
            (Source::MusicApp, Some(true), LookupMode::Automatic),
            (Source::MusicApp, None, LookupMode::AwaitChoice),
            (Source::MusicApp, Some(false), LookupMode::Manual),
            (Source::Browser, Some(true), LookupMode::Manual),
            (Source::Other, Some(true), LookupMode::Manual),
        ] {
            let mode = source.lookup_mode(choice);
            assert_eq!(mode, expected);
            let mut current: Option<ActiveTrack> = None;
            for number in 1..=5 {
                let key = TrackKey {
                    title: format!("合成曲{number}"),
                    artist: "演奏者".into(),
                    album: "合成单元测试".into(),
                    number,
                };
                let previous = current.as_ref().map(|t| (t.id, t.permit.clone()));
                let next = ActiveTrack::observe(
                    &mut current,
                    key.clone(),
                    source,
                    mode,
                    false,
                    &generation,
                    7,
                );
                if mode == LookupMode::Automatic {
                    assert_eq!(next, TrackChange::Automatic);
                    assert!(current.as_ref().unwrap().automatic);
                } else {
                    assert_eq!(next, TrackChange::Inactive);
                    assert!(current.is_none());
                    let requested = ActiveTrack::observe(
                        &mut current,
                        key.clone(),
                        source,
                        mode,
                        true,
                        &generation,
                        7,
                    );
                    if mode == LookupMode::AwaitChoice {
                        assert_eq!(requested, TrackChange::Inactive);
                        assert!(current.is_none());
                        continue;
                    }
                    assert_eq!(requested, TrackChange::Offer);
                    assert!(!current.as_ref().unwrap().automatic);
                }
                let id = current.as_ref().unwrap().id;
                if let Some((old, permit)) = previous {
                    assert_ne!(id, old);
                    assert!(!permit.valid());
                }
                assert_eq!(
                    ActiveTrack::observe(&mut current, key, source, mode, false, &generation, 7),
                    TrackChange::Same
                );
                assert_eq!(current.as_ref().unwrap().id, id);
                assert!(current.as_ref().unwrap().permit.valid());
            }
        }
    }

    #[test]
    fn queued_lookup_expires_on_session_change_or_track_revocation() {
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
