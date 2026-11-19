use crate::media::Media;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::Serialize;
use std::fmt::Write;
use std::sync::LazyLock;

const LIMIT: usize = 20_000;
static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{1,3}):([0-5]\d)(?:[.:](\d{1,3}))?\]").unwrap());
static OFFSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[offset:\s*([+-]?\d{1,8})\s*\]").unwrap());

#[derive(Debug)]
struct Cue {
    ms: i64,
    text: String,
}

fn cues(raw: &str) -> Vec<Cue> {
    let offset = OFFSET
        .captures(raw)
        .and_then(|c| c[1].parse::<i64>().ok())
        .unwrap_or(0);
    let mut result = Vec::new();
    for line in raw.lines() {
        let mut rest = line.trim_start_matches('\u{feff}').trim();
        let mut times = Vec::new();
        while let Some(c) = STAMP.captures(rest) {
            let minutes: i64 = c[1].parse().unwrap_or_default();
            let seconds: i64 = c[2].parse().unwrap_or_default();
            let fraction = c.get(3).map(|m| m.as_str()).unwrap_or("");
            let milli =
                fraction.parse::<i64>().unwrap_or(0) * 10_i64.pow(3 - fraction.len() as u32);
            times.push(((minutes * 60 + seconds) * 1000 + milli - offset).clamp(0, 59_999_999));
            rest = &rest[c[0].len()..];
        }
        let text = rest.trim().chars().take(320).collect::<String>();
        for ms in times {
            result.push(Cue {
                ms,
                text: text.clone(),
            });
        }
    }
    result.sort_by_key(|cue| cue.ms);
    result
}

#[derive(Debug)]
pub struct Lyrics {
    header: String,
    duration: f64,
    main: Vec<Cue>,
    translation: Vec<Cue>,
    pub provider: &'static str,
}

impl Lyrics {
    pub fn new(
        title: &str,
        duration: f64,
        raw: &str,
        translation: &str,
        provider: &'static str,
    ) -> Option<Self> {
        let main = cues(raw);
        if !main
            .iter()
            .any(|cue| !cue.text.is_empty() && cue.text != "//")
        {
            return None;
        }
        let title: String = title
            .replace(['[', ']', '\r', '\n'], " ")
            .chars()
            .take(320)
            .collect();
        Some(Self {
            header: format!("[ti:{title}]\n[length:{duration:.3}]\n"),
            duration,
            main,
            translation: cues(translation),
            provider,
        })
    }
    fn at(&self, m: &Media) -> i64 {
        let mut p = m.position.unwrap_or_default();
        let title = m.song.title.to_lowercase();
        if self.duration > 0.0
            && m.duration > self.duration * 1.55
            && ["循环", "loop", "repeat"].iter().any(|s| title.contains(s))
        {
            let period = m.duration / (m.duration / self.duration).round();
            if (period - self.duration).abs() < self.duration * 0.04 {
                p %= period;
            }
        }
        (p * 1000.0) as i64
    }
}

fn lrc(rows: &[Cue], header: &str) -> String {
    let mut text = String::from(header);
    for cue in rows {
        let ms = cue.ms;
        let _ = writeln!(
            text,
            "[{:02}:{:02}.{:03}]{}",
            ms / 60000,
            ms / 1000 % 60,
            ms % 1000,
            cue.text
        );
    }
    text
}

fn window(rows: &[Cue], at: i64, radius: usize) -> &[Cue] {
    let index = rows.partition_point(|cue| cue.ms <= at).saturating_sub(1);
    &rows[index.saturating_sub(radius)..(index + radius + 1).min(rows.len())]
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Packet<'a> {
    title: &'a str,
    artist: &'a str,
    sent: i64,
    position: Option<f64>,
    duration: f64,
    playing: bool,
    rate: f64,
    lyrics_text: &'a str,
    translated_text: &'a str,
}

fn encode(m: &Media, lyrics: &str, translation: &str) -> String {
    let packet = Packet {
        title: &m.song.title,
        artist: &m.song.artist,
        sent: m.sent,
        position: m.position,
        duration: m.duration,
        playing: m.playing,
        rate: m.rate,
        lyrics_text: lyrics,
        translated_text: translation,
    };
    STANDARD.encode(serde_json::to_vec(&packet).expect("serializable media packet"))
}

pub fn build(m: &Media, lyrics: Option<&Lyrics>) -> String {
    if let Some(words) = lyrics {
        let main = lrc(&words.main, &words.header);
        let translation = lrc(&words.translation, "");
        let full = encode(m, &main, &translation);
        if full.len() <= LIMIT {
            return full;
        }
        if !translation.is_empty() {
            let plain = encode(m, &main, "");
            if plain.len() <= LIMIT {
                return plain;
            }
        }
        let at = words.at(m);
        for radius in [24, 12, 6, 3, 1, 0] {
            let main = lrc(window(&words.main, at, radius), &words.header);
            let trans = lrc(window(&words.translation, at, radius), "");
            let value = encode(m, &main, &trans);
            if value.len() <= LIMIT {
                return value;
            }
        }
        return encode(m, &lrc(window(&words.main, at, 0), &words.header), "");
    }
    encode(m, "", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Song, Source};
    #[test]
    fn timestamps_offset_and_multiple_stamps() {
        let c = cues("[offset:150]\n[00:01.5][01:20.324]雨\n[00:02]停");
        assert_eq!(
            c.iter().map(|c| c.ms).collect::<Vec<_>>(),
            [1350, 1850, 80174]
        );
    }
    #[test]
    fn published_schema_and_long_song_seek() {
        let raw: String = (0..220)
            .map(|i| {
                format!(
                    "[{:02}:{:02}.00]行{i} {}\n",
                    i / 60,
                    i % 60,
                    "雨".repeat(120)
                )
            })
            .collect();
        let words = Lyrics::new("测试", 220.0, &raw, &raw, "测试").unwrap();
        let mut m = Media {
            song: Song {
                title: "《测试》".into(),
                artist: "歌手".into(),
                source: Source::Bilibili,
            },
            duration: 220.0,
            position: Some(174.2),
            playing: true,
            rate: 1.0,
            sent: 12345,
        };
        for sec in [174, 4] {
            m.position = Some(sec as f64 + 0.2);
            let value = build(&m, Some(&words));
            assert!(value.len() <= LIMIT);
            let p: serde_json::Value =
                serde_json::from_slice(&STANDARD.decode(value).unwrap()).unwrap();
            assert_eq!(p["title"], "《测试》");
            assert_eq!(p["sent"], 12345);
            assert!(
                p["lyricsText"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("行{sec} "))
            );
            assert!(p["lyricsText"].as_str().unwrap().starts_with("[ti:测试]"));
            assert!(p.get("translatedText").is_some());
        }
    }
}
