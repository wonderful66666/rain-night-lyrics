use crate::media::Media;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::Serialize;
use std::fmt::Write;
use std::sync::LazyLock;

const LIMIT: usize = 20_000;
const PACKET_BYTES: usize = LIMIT / 4 * 3;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 8192;
const MAX_LINE_BYTES: usize = 8192;
const MAX_TIMES_PER_LINE: usize = 64;
const MAX_CUES: usize = 4096;
const MAX_TEXT_BYTES: usize = 256 * 1024;
static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[([0-9]{1,3}):([0-5][0-9])(?:[.:]([0-9]{1,3}))?\]").unwrap());
static OFFSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[offset:\s*([+-]?\d{1,8})\s*\]").unwrap());

#[derive(Debug)]
struct Cue {
    ms: i64,
    text: String,
}

fn short_text(s: &str) -> &str {
    s.char_indices().nth(320).map_or(s, |(end, _)| &s[..end])
}

fn cues(raw: &str) -> Option<Vec<Cue>> {
    if raw.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let offset = OFFSET
        .captures(raw)
        .and_then(|c| c[1].parse::<i64>().ok())
        .unwrap_or(0);
    let mut result = Vec::new();
    let mut text_bytes = 0;
    for (index, line) in raw.lines().enumerate() {
        if index >= MAX_LINES || line.len() > MAX_LINE_BYTES {
            return None;
        }
        let mut rest = line.trim_start_matches('\u{feff}').trim();
        let mut times = Vec::new();
        while let Some(c) = STAMP.captures(rest) {
            if times.len() >= MAX_TIMES_PER_LINE || result.len() + times.len() >= MAX_CUES {
                return None;
            }
            let minutes: i64 = c[1].parse().unwrap_or_default();
            let seconds: i64 = c[2].parse().unwrap_or_default();
            let fraction = c.get(3).map(|m| m.as_str()).unwrap_or("");
            let milli =
                fraction.parse::<i64>().unwrap_or(0) * 10_i64.pow(3 - fraction.len() as u32);
            times.push(((minutes * 60 + seconds) * 1000 + milli - offset).clamp(0, 59_999_999));
            rest = &rest[c[0].len()..];
        }
        let text = short_text(rest.trim());
        text_bytes += times.len() * text.len();
        if text_bytes > MAX_TEXT_BYTES {
            return None;
        }
        for ms in times {
            result.push(Cue {
                ms,
                text: text.to_owned(),
            });
        }
    }
    result.sort_by_key(|cue| cue.ms);
    Some(result)
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
        let main = cues(raw)?;
        if !main
            .iter()
            .any(|cue| !cue.text.is_empty() && cue.text != "//")
        {
            return None;
        }
        let title = short_text(title).replace(['[', ']', '\r', '\n'], " ");
        Some(Self {
            header: format!("[ti:{title}]\n[length:{duration:.3}]\n"),
            duration,
            main,
            translation: cues(translation).unwrap_or_default(),
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
    pub fn displayed(&self, m: &Media, delay: i64) -> Option<(&str, &str)> {
        m.position?;
        let at = self.at(m) + delay;
        let end = self.main.partition_point(|cue| cue.ms <= at);
        let cue = self.main.get(end.checked_sub(1)?)?;
        if cue.text.is_empty()
            || cue.text == "//"
            || (at - cue.ms > 9000
                && self
                    .main
                    .get(end)
                    .is_none_or(|next| next.ms - cue.ms > 12000))
        {
            return None;
        }
        let i = self.translation.partition_point(|row| row.ms <= cue.ms);
        let translated = self
            .translation
            .get(i)
            .into_iter()
            .chain(i.checked_sub(1).and_then(|j| self.translation.get(j)))
            .min_by_key(|row| row.ms.abs_diff(cue.ms))
            .filter(|r| r.ms.abs_diff(cue.ms) <= 500 && r.text != "//" && r.text != cue.text)
            .map_or("", |r| r.text.as_str());
        Some((&cue.text, translated))
    }
}

fn lrc(rows: &[Cue], header: &str) -> Option<String> {
    let mut text = String::from(header);
    for cue in rows {
        if text.len() + cue.text.len() > PACKET_BYTES {
            return None;
        }
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
    (text.len() <= PACKET_BYTES).then_some(text)
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
        title: short_text(&m.song.title),
        artist: "",
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

const METADATA_LRC: &str = "[ti:metadata]\n[offset:60000]\n[00:00.000]♪\n[00:00.001]\n";

pub fn metadata_only(m: &Media) -> String {
    let mut current = m.clone();
    current.sent = crate::media::now_ms();
    current.position = None;
    encode(&current, METADATA_LRC, "")
}

pub fn build(m: &Media, lyrics: Option<&Lyrics>) -> String {
    if let Some(words) = lyrics {
        if let Some(main) = lrc(&words.main, &words.header) {
            if let Some(translation) = lrc(&words.translation, "")
                && !translation.is_empty()
            {
                let full = encode(m, &main, &translation);
                if full.len() <= LIMIT {
                    return full;
                }
            }
            let plain = encode(m, &main, "");
            if plain.len() <= LIMIT {
                return plain;
            }
        }
        let at = words.at(m);
        for radius in [24, 12, 6, 3, 1, 0] {
            let Some(main) = lrc(window(&words.main, at, radius), &words.header) else {
                continue;
            };
            let Some(trans) = lrc(window(&words.translation, at, radius), "") else {
                continue;
            };
            let value = encode(m, &main, &trans);
            if value.len() <= LIMIT {
                return value;
            }
        }
        if let Some(main) = lrc(window(&words.main, at, 0), &words.header) {
            let plain = encode(m, &main, "");
            if plain.len() <= LIMIT {
                return plain;
            }
        }
    }
    encode(m, METADATA_LRC, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Song, Source};
    #[test]
    fn unverified_recording_replaces_stale_lyrics_without_sending_match_evidence() {
        let mut m = Media::anonymous(true);
        m.song.title = "Silver Meadow".into();
        m.song.artist = "local-only performer".into();
        m.position = Some(0.0);
        m.sent = 480;
        let packet: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(build(&m, None)).unwrap()).unwrap();
        assert_eq!(packet["title"], m.song.title);
        assert_eq!(packet["artist"], "");
        assert_eq!(packet["sent"], m.sent);
        assert_eq!(packet["position"], 0.0);
        assert_eq!(packet["translatedText"], "");
        let neutral = Lyrics::new(
            "metadata",
            0.0,
            packet["lyricsText"].as_str().unwrap(),
            "",
            "测试",
        )
        .unwrap();
        for position in [0.0, 0.001, 3.0, 30.0, 180.0] {
            m.position = Some(position);
            for delay in [-30_000, 0, 30_000] {
                assert!(neutral.displayed(&m, delay).is_none());
            }
        }
    }
    #[test]
    fn disc_anchor_uses_the_same_cue_delay_translation_and_long_silences() {
        let words = Lyrics::new(
            "曲",
            40.0,
            "[00:02]第一句\n[00:20]第二句",
            "[00:02.25]翻译\n[00:22]太远",
            "测试",
        )
        .unwrap();
        let mut m = Media {
            song: Song {
                title: "曲".into(),
                artist: String::new(),
                source: Source::Other,
            },
            position: Some(1.8),
            duration: 40.0,
            rate: 1.0,
            playing: true,
            sent: 0,
        };
        assert_eq!(words.displayed(&m, 0), None);
        assert_eq!(words.displayed(&m, 250), Some(("第一句", "翻译")));
        m.position = Some(12.0);
        assert_eq!(words.displayed(&m, 0), None);
        m.position = Some(21.0);
        assert_eq!(words.displayed(&m, 0), Some(("第二句", "")));
        m.position = None;
        assert_eq!(words.displayed(&m, 0), None);
    }
    #[test]
    fn timestamps_offset_and_multiple_stamps() {
        let c = cues("[offset:150]\n[00:01.5][01:20.324]雨\n[00:02]停").unwrap();
        assert_eq!(
            c.iter().map(|c| c.ms).collect::<Vec<_>>(),
            [1350, 1850, 80174]
        );
    }
    #[test]
    fn parsing_enforces_source_and_expanded_lyric_budgets() {
        assert!(cues(&"x".repeat(MAX_SOURCE_BYTES + 1)).is_none());
        assert!(cues(&"x".repeat(MAX_LINE_BYTES + 1)).is_none());
        assert!(cues(&"\n".repeat(MAX_LINES + 1)).is_none());
        assert!(cues(&"[00:01]".repeat(MAX_TIMES_PER_LINE + 1)).is_none());
        assert!(cues(&"[00:01]a\n".repeat(MAX_CUES + 1)).is_none());
        let repeated = format!(
            "{}{}\n",
            "[00:01]".repeat(MAX_TIMES_PER_LINE),
            "雨".repeat(320)
        );
        assert!(cues(&repeated).is_some());
        let oversized = repeated.repeat(5);
        assert!(cues(&oversized).is_none());
        let main = Lyrics::new("曲", 1.0, "[00:00]雨", &oversized, "测试").unwrap();
        assert_eq!(main.main.len(), 1);
        assert!(main.translation.is_empty());
        assert!(cues("[00:01.１２３]雨").unwrap().is_empty());
    }
    #[test]
    fn encoded_packets_remain_bounded_with_escaped_and_multibyte_fields() {
        let m = Media {
            song: Song {
                title: "\0".repeat(20_000),
                artist: "🌂".repeat(20_000),
                source: Source::NetEase,
            },
            position: Some(5.0),
            duration: 80.0,
            playing: true,
            rate: 1.0,
            sent: 10000,
        };
        let line = format!("[00:05]{}\n", "\u{1}".repeat(320));
        let words = Lyrics::new(
            &m.song.title,
            80.0,
            &line.repeat(40),
            &line.repeat(40),
            "测试",
        )
        .unwrap();
        for packet in [build(&m, None), build(&m, Some(&words)), metadata_only(&m)] {
            assert!(packet.len() <= LIMIT);
            let value: serde_json::Value =
                serde_json::from_slice(&STANDARD.decode(packet).unwrap()).unwrap();
            assert_eq!(value["artist"], "");
            assert_eq!(value["title"], "\0".repeat(320));
        }
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
                source: Source::Browser,
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
        let retire = metadata_only(&m);
        let packet: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(retire).unwrap()).unwrap();
        let retire_lrc = packet["lyricsText"].as_str().unwrap();
        assert!(packet["position"].is_null());
        assert_eq!(packet["title"], m.song.title);
        let clear = Lyrics::new("metadata", 0.0, retire_lrc, "", "测试").unwrap();
        assert!(clear.displayed(&m, 0).is_none());
    }
}
