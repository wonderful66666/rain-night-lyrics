mod lookup;
mod media;
mod protocol;
mod wallpaper;

use lookup::{Catalog, Outcome};
use media::{Apartment, Media, Sessions, Song};
use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};
use wallpaper::{Engine, SingleInstance, StopSignal, WORKSHOP_ID};

struct CacheEntry {
    until: Instant,
    duration: f64,
    result: Outcome,
}

fn duration_changed(old: f64, new: f64) -> bool {
    new > 0.0 && (old <= 0.0 || (new - old).abs() > 2.0)
}

struct Worker {
    revision: Arc<AtomicU64>,
    requests: mpsc::Sender<(u64, Media)>,
    results: mpsc::Receiver<(u64, Outcome)>,
    pending: bool,
}

impl Worker {
    fn new() -> Self {
        let revision = Arc::new(AtomicU64::new(1));
        let current = Arc::clone(&revision);
        let (send, receive) = mpsc::channel::<(u64, Media)>();
        let (return_result, results) = mpsc::channel();
        thread::spawn(move || {
            let catalog = Catalog::new();
            while let Ok(first) = receive.recv() {
                let (version, media) = receive.try_iter().last().unwrap_or(first);
                let valid = || current.load(Ordering::Acquire) == version;
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
    fn request(&mut self, data: &Media) {
        if !self.pending {
            self.pending = self
                .requests
                .send((self.revision.load(Ordering::Acquire), data.clone()))
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

fn message(last: &mut String, next: &str) {
    if last != next {
        println!("{next}");
        last.clear();
        last.push_str(next);
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
    match Sessions::open().and_then(|s| s.current()) {
        Ok(Some(m)) => println!(
            "Windows 媒体来源 {}　正在播放 {}　有进度 {}",
            m.song.source.label(),
            m.playing,
            m.position.is_some()
        ),
        Ok(None) => println!("Windows 当前没有播放或暂停的媒体会话。"),
        Err(_) => return Err("无法读取 Windows 媒体会话。".into()),
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let _instance = match SingleInstance::acquire().map_err(|_| "无法检查取词器状态。")? {
        Some(instance) => instance,
        None => {
            println!("已有取词器在运行。");
            check()?;
            if io::stdin().is_terminal() {
                println!("按回车关闭本窗口。输入 q 并回车可停止正在运行的 Rust 取词器。");
                let mut answer = String::new();
                if io::stdin().read_line(&mut answer).is_ok()
                    && answer.trim().eq_ignore_ascii_case("q")
                {
                    StopSignal::send();
                }
            }
            return Ok(());
        }
    };
    let stop = StopSignal::create().map_err(|_| "无法创建退出信号。")?;
    println!(
        "雨夜轻息 · 独立取词器 v{}　关闭窗口或按 Ctrl+C 退出。",
        env!("CARGO_PKG_VERSION")
    );
    let mut sessions = None;
    let mut engine = None;
    let mut screens = Vec::new();
    let mut poll_engine = Instant::now();
    let mut worker = Worker::new();
    let mut cache = HashMap::<Song, CacheEntry>::new();
    let mut song = None;
    let mut sent: Option<Media> = None;
    let mut status = String::new();
    while !stop.requested() {
        let now = Instant::now();
        if now >= poll_engine {
            let active = Engine::find();
            let locations = active.as_ref().map(Engine::displays).unwrap_or_default();
            if engine != active || locations != screens {
                sent = None;
            }
            engine = active;
            screens = locations;
            poll_engine = now + Duration::from_secs(2);
        }
        if screens.is_empty() {
            if song.take().is_some() {
                worker.cancel();
            }
            sessions = None;
            message(
                &mut status,
                if engine.is_none() {
                    "等待 Wallpaper Engine 启动。"
                } else {
                    "等待桌面选用工坊订阅的《雨夜轻息》。"
                },
            );
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let engine = engine.as_ref().unwrap();
        if sessions.is_none() {
            sessions = Sessions::open().ok();
        }
        let Some(manager) = sessions.as_ref() else {
            message(&mut status, "Windows 媒体服务暂时不可用，正在重试。");
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        let current = match manager.current() {
            Ok(m) => m,
            Err(_) => {
                sessions = None;
                message(&mut status, "正在重新连接 Windows 媒体会话。");
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let Some(m) = current else {
            if song.take().is_some() {
                worker.cancel();
                for display in &screens {
                    engine.send(display, "");
                }
                sent = None;
            }
            message(&mut status, "已连接工坊版，等待 Windows 播放媒体。");
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        if song.as_ref().is_none_or(|(previous, duration)| {
            previous != &m.song || duration_changed(*duration, m.duration)
        }) {
            worker.cancel();
            song = Some((m.song.clone(), m.duration));
            sent = None;
            status.clear();
        }
        if let Some(result) = worker.ready() {
            let seconds = match &result {
                Outcome::Found(_) => 21600,
                Outcome::Missing => 600,
                Outcome::Unavailable => 45,
            };
            cache.retain(|_, entry| entry.until > now);
            if cache.len() >= 64 {
                if let Some(key) = cache
                    .iter()
                    .min_by_key(|(_, e)| e.until)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&key);
                }
            }
            cache.insert(
                m.song.clone(),
                CacheEntry {
                    until: now + Duration::from_secs(seconds),
                    duration: m.duration,
                    result,
                },
            );
            sent = None;
        }
        if cache
            .get(&m.song)
            .is_some_and(|e| e.until <= now || duration_changed(e.duration, m.duration))
        {
            cache.remove(&m.song);
            sent = None;
        }
        let entry = cache.get(&m.song);
        if entry.is_none() && m.playing {
            worker.request(&m);
        }
        let words = entry.and_then(|e| {
            if let Outcome::Found(w) = &e.result {
                Some(w)
            } else {
                None
            }
        });
        let mut delivered = true;
        if sent.as_ref().is_none_or(|old| m.needs_sync(old)) {
            let packet = protocol::build(&m, words);
            for display in &screens {
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
        let state = if !delivered {
            "壁纸切换中，正在重连".to_owned()
        } else {
            match entry.map(|e| &e.result) {
                Some(Outcome::Found(w)) => format!("已把{}的同步歌词送到壁纸", w.provider),
                Some(Outcome::Missing) => "曲库未找到匹配的同步歌词，胶囊显示歌名".into(),
                Some(Outcome::Unavailable) => "查词服务暂时未返回有效结果，稍后重试".into(),
                None if !m.playing => "播放后开始查词".into(),
                None => "正在查找同步歌词".into(),
            }
        };
        let playback = if m.playing { "播放中" } else { "已暂停" };
        let clock = if m.position.is_none() {
            " · 播放器未提供进度"
        } else {
            ""
        };
        message(
            &mut status,
            &format!("{} · {playback} · {state}{clock}", m.song.source.label()),
        );
        thread::sleep(Duration::from_millis(480));
    }
    worker.cancel();
    Ok(())
}

fn main() {
    let option = std::env::args().nth(1);
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
    let result = Apartment::open()
        .map_err(|_| "无法连接 Windows 运行时。".into())
        .and_then(|_apartment| if option.is_some() { check() } else { run() });
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
                source: Source::Bilibili,
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
