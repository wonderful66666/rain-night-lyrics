#![windows_subsystem = "windows"]
mod animation;
mod audio;
mod desktop;
mod lookup;
mod lookup_dialog;
mod media;
mod placement;
mod protocol;
mod render;
mod ui;
mod wallpaper;

use audio::OutputWatch;
use desktop::{UiBridge, UiSnapshot};
use lookup::{Catalog, Outcome};
use media::{Apartment, ApprovedTrack, LookupPermit, Media, Sessions, Song};
use placement::CapsuleData;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};
use wallpaper::{Engine, ReloadReason, SingleInstance, StopSignal, WORKSHOP_ID};

struct CacheEntry {
    until: Instant,
    result: Outcome,
}

fn duration_changed(old: f64, new: f64) -> bool {
    new > 0.0 && (old <= 0.0 || (new - old).abs() > 2.0)
}

fn lookup_changed(current: Option<(u64, f64)>, id: u64, duration: f64) -> bool {
    current.is_none_or(|(previous, old_duration)| {
        previous != id || duration_changed(old_duration, duration)
    })
}

struct Worker {
    revision: Arc<AtomicU64>,
    requests: mpsc::Sender<(u64, Media, LookupPermit)>,
    results: mpsc::Receiver<(u64, Outcome)>,
    pending: bool,
}

impl Worker {
    fn new() -> Self {
        let revision = Arc::new(AtomicU64::new(1));
        let current = Arc::clone(&revision);
        let (send, receive) = mpsc::channel::<(u64, Media, LookupPermit)>();
        let (return_result, results) = mpsc::channel();
        thread::spawn(move || {
            let catalog = Catalog::new();
            while let Ok(first) = receive.recv() {
                let (version, media, permit) = receive.try_iter().last().unwrap_or(first);
                let valid = || current.load(Ordering::Acquire) == version && permit.valid();
                if !valid() {
                    continue;
                }
                let result = catalog.lookup(&media, valid);
                if valid() && return_result.send((version, result)).is_err() {
                    break;
                }
            }
        });
        Self {
            revision,
            requests: send,
            results,
            pending: false,
        }
    }
    fn cancel(&mut self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.pending = false;
    }
    fn request(&mut self, lookup: &Media, permit: &LookupPermit) {
        if !self.pending && permit.valid() {
            self.pending = self
                .requests
                .send((
                    self.revision.load(Ordering::Acquire),
                    lookup.clone(),
                    permit.clone(),
                ))
                .is_ok();
        }
    }
    fn ready(&mut self) -> Option<Outcome> {
        let version = self.revision.load(Ordering::Acquire);
        let found = self
            .results
            .try_iter()
            .find_map(|(v, r)| (v == version).then_some(r));
        if found.is_some() {
            self.pending = false;
        }
        found
    }
}

fn check() -> Result<(), String> {
    let engine = Engine::find();
    let screens = engine.as_ref().map(Engine::displays).unwrap_or_default();
    println!(
        "工坊编号 {WORKSHOP_ID}　播放器运行 {}　匹配屏幕 {}",
        engine.is_some(),
        screens.len()
    );
    if let Some((hz, fps)) = ui::frame_stats() {
        println!("桌面圆圈　屏幕刷新率 {hz:.1} Hz　最近绘制 {fps:.1} 帧/秒");
    }
    match OutputWatch::open().and_then(|w| w.status()) {
        Ok((notifications, output)) => println!(
            "音柱重连　设备通知 {}　默认输出 {}",
            if notifications {
                "可用"
            } else {
                "使用轮询"
            },
            match output {
                None => "未连接",
                Some(true) => "当前有声音",
                Some(false) => "当前未测到声音",
            }
        ),
        Err(_) => println!("音柱重连　暂时无法读取默认输出设备。"),
    }
    let mut sessions = Sessions::open().map_err(|_| "无法读取 Windows 媒体会话。")?;
    sessions
        .current(false, None)
        .map_err(|_| "无法读取 Windows 媒体会话。")?;
    println!("当前媒体可逐首确认查词　{}", sessions.eligible);
    Ok(())
}

fn core(bridge: &UiBridge) -> Result<(), String> {
    let _apartment = Apartment::open().map_err(|_| "无法连接 Windows 媒体服务。")?;
    let mut sessions = None;
    let mut engine = None;
    let mut screens = Vec::new();
    let mut audio = None;
    let mut poll_engine = Instant::now();
    let mut worker = Worker::new();
    let mut cached: Option<CacheEntry> = None;
    let mut song = None;
    let mut sent: Option<Media> = None;
    let mut art = None;
    let mut art_key = None;
    let mut retry_art = Instant::now();
    let mut offered = None;
    let mut last_track: Option<(Song, Instant)> = None;
    let mut notice_until = None;
    let mut scene = CapsuleData::default();
    let mut delay = 0;
    while !bridge.finished.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= poll_engine {
            let active = Engine::find();
            let (locations, geometry, offset) =
                active.as_ref().map(Engine::scene).unwrap_or_default();
            scene = geometry;
            delay = offset;
            if engine != active || locations != screens {
                sent = None;
                audio = None;
                if locations.is_empty() && !screens.is_empty() {
                    worker.cancel();
                }
            }
            if !locations.is_empty() && audio.is_none() {
                audio = OutputWatch::open().ok();
            }
            engine = active;
            screens = locations;
            poll_engine = now + Duration::from_secs(2);
        }
        let waiting = if engine.is_none() {
            Some("等待 Wallpaper Engine")
        } else if screens.is_empty() {
            Some("未使用雨夜轻息")
        } else {
            None
        };
        let idle = |status| UiSnapshot {
            status: waiting.unwrap_or(status),
            placement: CapsuleData {
                title: "正在播放".into(),
                ..scene.clone()
            },
            ..Default::default()
        };
        if let (Some(watch), Some(engine)) = (&mut audio, &engine) {
            match watch.ready(now) {
                Ok(true) => {
                    let (count, retry) = engine.reload(&screens, ReloadReason::AudioOutput);
                    if count > 0 {
                        desktop::log("音频输出切换后有声，已请求重新载入场景");
                        sent = None;
                    }
                    if retry {
                        watch.retry(Instant::now());
                    }
                }
                Err(_) => audio = None,
                Ok(false) => {}
            }
        }
        if waiting.is_none() && sessions.is_none() {
            sessions = Sessions::open().ok();
        }
        let request = bridge.take_lookup_request();
        let answer = bridge.take_lookup_answer();
        let current = match sessions.as_mut() {
            Some(manager) if waiting.is_none() => match manager.current(request, answer) {
                Ok(m) => m,
                Err(_) => {
                    sessions = None;
                    None
                }
            },
            _ => {
                sessions = None;
                None
            }
        };
        let offer = sessions.as_ref().and_then(Sessions::offer);
        if offer.as_ref().map(|(id, _)| *id) != offered {
            offered = offer.as_ref().map(|(id, _)| *id);
            bridge.offer_lookup_title(offer);
        }
        let Some(ApprovedTrack {
            media: m,
            permit,
            id,
            lookup_media,
        }) = current.filter(|track| track.permit.valid())
        else {
            if song.take().is_some() {
                worker.cancel();
                if let Some(engine) = &engine {
                    clear_automatic(engine, &screens, sent.as_ref());
                }
                sent = None;
            }
            cached = None;
            last_track = None;
            notice_until = None;
            art = None;
            art_key = None;
            let mut snapshot = idle(
                if sessions.as_ref().is_some_and(|manager| manager.eligible) {
                    "右键可查本首歌词"
                } else {
                    "等待播放"
                },
            );
            snapshot.media = sessions
                .as_ref()
                .and_then(|s| s.local_playing)
                .map(Media::anonymous);
            bridge.publish(snapshot);
            thread::park_timeout(Duration::from_millis(500));
            continue;
        };
        if lookup_changed(song, id, lookup_media.duration) {
            worker.cancel();
            song = Some((id, lookup_media.duration));
            cached = None;
            sent = None;
        }
        if let Some((old, when)) = &last_track
            && old != &m.song
        {
            notice_until = (now.duration_since(*when) < Duration::from_secs(5))
                .then_some(now + Duration::from_millis(3450));
        }
        last_track = Some((m.song.clone(), now));
        let key = (m.song.clone(), bridge.desired_size());
        if art_key.as_ref() != Some(&key) || (art.is_none() && now >= retry_art) {
            art = sessions
                .as_ref()
                .and_then(|manager| manager.cover(key.1))
                .map(Arc::new);
            art_key = Some(key);
            retry_art = now + Duration::from_secs(15);
        }
        if let Some(result) = worker.ready() {
            if let Outcome::Found(words) = &result {
                desktop::log(words.provider)
            }
            let seconds = match &result {
                Outcome::Found(_) => 21600,
                Outcome::Ambiguous | Outcome::Missing => 600,
                Outcome::Unavailable => 45,
            };
            cached = Some(CacheEntry {
                until: now + Duration::from_secs(seconds),
                result,
            });
            sent = None;
        }
        if cached.as_ref().is_some_and(|e| e.until <= now) {
            cached = None;
            sent = None;
        }
        let entry = cached.as_ref();
        if entry.is_none() && m.playing && waiting.is_none() {
            worker.request(&lookup_media, &permit);
        }
        let words = entry.and_then(|e| {
            if let Outcome::Found(w) = &e.result {
                Some(w)
            } else {
                None
            }
        });
        if !permit.valid() {
            continue;
        }
        let mut delivered = true;
        if let Some(engine) = &engine
            && !screens.is_empty()
            && sent.as_ref().is_none_or(|old| m.needs_sync(old))
        {
            let packet = protocol::build(&m, words);
            for display in &screens {
                if !permit.valid() {
                    delivered = false;
                    break;
                }
                if !engine.send(display, &packet) {
                    delivered = false;
                }
            }
            if delivered {
                sent = Some(m.clone());
            } else {
                poll_engine = Instant::now();
            }
        }
        if !permit.valid() {
            continue;
        }
        let state = if !delivered {
            "正在重连壁纸"
        } else if !m.playing {
            "已暂停"
        } else {
            match entry.map(|e| &e.result) {
                Some(Outcome::Found(_)) => "歌词已同步",
                Some(Outcome::Ambiguous) => "未确认歌曲版本",
                Some(Outcome::Missing) => "没找到歌词",
                Some(Outcome::Unavailable) => "查词服务忙",
                None => "正在找歌词",
            }
        };
        let (title, subtitle) = words
            .and_then(|w| w.displayed(&m, delay))
            .unwrap_or((&m.song.title, &m.song.artist));
        let placement = CapsuleData {
            title: title.to_owned(),
            subtitle: subtitle.to_owned(),
            notice: notice_until
                .filter(|until| *until > now)
                .map(|_| ("正在播放".into(), m.song.title.clone())),
            ..scene.clone()
        };
        bridge.publish(UiSnapshot {
            media: Some(m),
            cover: art.clone(),
            status: waiting.unwrap_or(state),
            placement,
        });
        thread::park_timeout(Duration::from_millis(480));
    }
    worker.cancel();
    if let Some(engine) = Engine::find() {
        clear_automatic(&engine, &engine.displays(), sent.as_ref());
    }
    Ok(())
}

fn clear_automatic(engine: &Engine, displays: &[String], last: Option<&Media>) {
    if let Some(media) = last {
        let mut wait = false;
        let packet = protocol::metadata_only(media);
        for display in displays {
            if engine.in_playlist(display) {
                wait |= engine.send(display, &packet);
            }
        }
        if wait {
            thread::sleep(Duration::from_millis(420));
        }
    }
    let mut cleared_displays = Vec::new();
    for display in displays {
        let mut cleared = engine.send(display, "");
        if !cleared && engine.displays().contains(display) {
            thread::sleep(Duration::from_millis(100));
            cleared = engine.send(display, "");
        }
        if cleared {
            cleared_displays.push(display.clone())
        } else {
            desktop::log("清空歌词时场景不可用");
        }
    }
    let (count, retry) = engine.reload(&cleared_displays, ReloadReason::LyricsCleared);
    if count > 0 {
        desktop::log("已清空自动歌词并请求刷新当前场景");
    }
    if retry {
        desktop::log("场景刷新命令未完成");
    }
}

fn run() -> Result<(), String> {
    let show = StopSignal::show_event().map_err(|_| "无法创建桌面提示事件。")?;
    let Some(_single) = SingleInstance::acquire().map_err(|_| "无法检查取词器状态。")?
    else {
        StopSignal::show();
        return Ok(());
    };
    let stop = StopSignal::create().map_err(|_| "无法创建退出事件。")?;
    let bridge = Arc::new(UiBridge::default());
    if let Some(engine) = Engine::find() {
        let (_, placement, _) = engine.scene();
        bridge.publish(UiSnapshot {
            status: if placement.display.is_some() {
                "等待播放"
            } else {
                "未使用雨夜轻息"
            },
            placement,
            ..UiSnapshot::default()
        });
    }
    let worker = Arc::clone(&bridge);
    let core_thread = thread::spawn(move || core(&worker));
    let result = ui::run(Arc::clone(&bridge), stop.handle(), show.handle());
    bridge.finished.store(true, Ordering::Release);
    core_thread.thread().unpark();
    core_thread.join().map_err(|_| "媒体线程意外退出。")??;
    result.map_err(|_| "无法显示桌面圆圈。".into())
}

fn main() {
    let option = std::env::args().nth(1);
    if option.is_some() {
        unsafe {
            use windows::Win32::System::Console::*;
            let saved = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
                .map(|k| (k, GetStdHandle(k).ok().filter(|h| !h.is_invalid())));
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            for (k, handle) in saved {
                if let Some(handle) = handle {
                    let _ = SetStdHandle(k, handle);
                }
            }
        }
    }
    if option.as_deref() == Some("--stop") {
        println!(
            "{}",
            if StopSignal::send() {
                "已通知 Rust 取词器退出。"
            } else {
                "当前没有运行 Rust 取词器。"
            }
        );
        return;
    }
    if option.as_deref().is_some_and(|arg| arg != "--check") {
        println!("RainNightLyrics.exe [--check | --stop]");
        return;
    }
    let result = if option.is_none() {
        run()
    } else {
        Apartment::open()
            .map_err(|_| "无法连接 Windows 运行时。".into())
            .and_then(|_apartment| check())
    };
    if let Err(msg) = result {
        eprintln!("{msg}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Source;
    #[test]
    fn late_timeline_reopens_an_ambiguous_search() {
        assert!(duration_changed(0.0, 204.133));
        assert!(duration_changed(160.5, 204.133));
        assert!(!duration_changed(204.133, 204.18));
        assert!(!duration_changed(204.133, 0.0));
        assert!(lookup_changed(Some((6, 204.133)), 7, 204.133));
        assert!(!lookup_changed(Some((7, 204.133)), 7, 204.133));
    }

    #[test]
    fn old_lookup_never_crosses_a_song_change() {
        let (req, _) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let mut w = Worker {
            revision: Arc::new(AtomicU64::new(1)),
            requests: req,
            results: rx,
            pending: true,
        };
        w.cancel();
        w.pending = true;
        tx.send((1, Outcome::Unavailable)).unwrap();
        assert!(w.ready().is_none());
        assert!(w.pending);
        tx.send((2, Outcome::Missing)).unwrap();
        assert!(matches!(w.ready(), Some(Outcome::Missing)));
        assert!(!w.pending);
    }

    #[test]
    fn pause_seek_switch_and_heartbeat_are_sent() {
        let old = Media {
            song: Song {
                title: "夜雨".into(),
                artist: "演奏者".into(),
                source: Source::Browser,
            },
            duration: 100.0,
            position: Some(40.0),
            playing: true,
            rate: 1.0,
            sent: 1000,
        };
        let mut now = old.clone();
        now.sent += 500;
        now.position = Some(40.5);
        assert!(!now.needs_sync(&old));
        now.position = Some(12.0);
        assert!(now.needs_sync(&old));
        now.position = Some(40.5);
        now.playing = false;
        assert!(now.needs_sync(&old));
        now.playing = true;
        now.song.source = Source::NetEase;
        assert!(now.needs_sync(&old));
        now.song = old.song.clone();
        now.sent += 5000;
        assert!(now.needs_sync(&old));
    }
}
