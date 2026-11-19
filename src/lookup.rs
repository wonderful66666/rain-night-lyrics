use crate::media::{Media, Source};
use crate::protocol::Lyrics;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

static SITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*(?:[_|｜\s-]+(?:哔哩哔哩|bilibili|网易云音乐).*)$").unwrap()
});
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[《「『“"]([^》」』”"\n]{1,120})[》」』”"]"#).unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"【([^】]{1,120})】").unwrap());
static DIVIDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+[-–—]\s+|\s*[|｜丨]\s*").unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[（(]\s*(?:official (?:music )?(?:video|audio)|lyrics?(?: video)?|mv|hd|4k|高音质|无损音质)\s*[）)]").unwrap()
});

fn key(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            0xff01..=0xff5e => char::from_u32(c as u32 - 0xfee0).unwrap(),
            _ => c,
        })
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn names(title: &str, video: bool) -> Vec<String> {
    let plain = SITE.replace(title, "");
    let mut result = Vec::<String>::new();
    let mut add = |s: &str| {
        let cleaned = LABEL.replace_all(s, "");
        let name = cleaned
            .trim()
            .trim_matches(['【', '】', '《', '》', '「', '」', '"'])
            .trim();
        if !(1..=120).contains(&name.chars().count()) {
            return;
        }
        let n = key(name);
        if n.is_empty()
            || [
                "4k",
                "mv",
                "hd",
                "hires",
                "musicvideo",
                "officialaudio",
                "lyrics",
                "音乐",
                "单曲循环",
                "循环单曲",
                "纯享",
                "高音质",
                "双语字幕",
            ]
            .contains(&n.as_str())
        {
            return;
        }
        if !result.iter().any(|s| key(s) == n) {
            result.push(name.to_owned());
        }
    };
    if video {
        for c in QUOTED.captures_iter(&plain) {
            add(&c[1]);
        }
        let stripped = TAG.replace_all(&plain, " ");
        let parts: Vec<_> = DIVIDER.split(stripped.trim()).collect();
        if parts.len() > 1 {
            add(parts[1]);
        }
        for p in parts {
            if !QUOTED.is_match(p) {
                add(p);
            }
        }
        for c in TAG.captures_iter(&plain) {
            add(&c[1]);
        }
    }
    add(&plain);
    result.truncate(4);
    result
}

#[derive(Clone)]
struct Candidate {
    title: String,
    artist: String,
    seconds: f64,
    id: u64,
    lrc: String,
}

fn score(m: &Media, titles: &[String], s: &Candidate) -> Option<i32> {
    let name = key(&s.title);
    let index = titles.iter().position(|n| key(n) == name)?;
    let artist = key(&m.song.artist);
    let performer = key(&s.artist);
    let artist_matches = !artist.is_empty()
        && !performer.is_empty()
        && (performer.contains(&artist) || artist.contains(&performer));
    if !m.song.source.from_video() && !artist.is_empty() && !performer.is_empty() && !artist_matches
    {
        return None;
    }
    let looped = m.song.title.to_lowercase();
    let repeated = m.duration > 420.0
        && ["循环", "loop", "repeat"]
            .iter()
            .any(|s| looped.contains(s));
    let mut value = 100_000 - 5_000 * index as i32;
    if m.duration > 0.0 && s.seconds > 0.0 && !repeated {
        let difference = (s.seconds - m.duration).abs();
        if difference > if artist_matches { 35.0 } else { 18.0 } {
            return None;
        }
        value += ((22.0 - difference).max(0.0) * 1_000.0).round() as i32;
    }
    if artist_matches {
        value += if artist == performer { 45_000 } else { 40_000 };
    }
    if performer.chars().count() >= 2
        && m.song.source.from_video()
        && (titles.iter().any(|n| key(n) == performer) || key(&m.song.title).contains(&performer))
    {
        value += 25_000;
    }
    Some(value)
}

fn ranked_songs(m: &Media, titles: &[String], candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut ranked: Vec<_> = candidates
        .into_iter()
        .filter_map(|c| Some((score(m, titles, &c)?, c)))
        .collect();
    ranked.sort_by_key(|(value, _)| std::cmp::Reverse(*value));
    let Some((top, first)) = ranked.first() else {
        return vec![];
    };
    let performer = key(&first.artist);
    if (m.song.source.from_video() || m.song.artist.is_empty())
        && !performer.is_empty()
        && ranked
            .iter()
            .skip(1)
            .take_while(|(value, _)| top - value <= 350)
            .any(|(_, other)| !other.artist.is_empty() && key(&other.artist) != performer)
    {
        return vec![];
    }
    ranked
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| performer.is_empty() || key(&c.artist) == performer)
        .take(2)
        .collect()
}

fn json(s: &Value, name: &str) -> String {
    s[name].as_str().unwrap_or("").to_owned()
}

#[derive(Debug)]
pub enum Outcome {
    Found(Lyrics),
    Missing,
    Unavailable,
}

#[derive(Clone, Copy, PartialEq)]
enum Provider {
    NetEase,
    LrcLib,
}

pub struct Catalog(ureq::Agent);
impl Catalog {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(5)))
            .user_agent("RainNightLyrics/0.1")
            .build();
        Self(config.into())
    }

    fn get(&self, url: &str, params: &[(&str, &str)]) -> Result<Value, ()> {
        let mut request = self.0.get(url);
        if url.starts_with("https://music.163.com/") {
            request = request.header("Referer", "https://music.163.com/");
        }
        for (k, v) in params {
            request = request.query(*k, *v);
        }
        request
            .call()
            .map_err(|_| ())?
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_json()
            .map_err(|_| ())
    }

    fn search(&self, provider: Provider, title: &str, artist: &str) -> Result<Vec<Candidate>, ()> {
        let mut out = Vec::new();
        if provider == Provider::NetEase {
            let term = format!("{title} {artist}");
            let body = self.get(
                "https://music.163.com/api/cloudsearch/pc",
                &[
                    ("s", term.trim()),
                    ("type", "1"),
                    ("limit", "12"),
                    ("offset", "0"),
                ],
            )?;
            if body["code"].as_i64() != Some(200) {
                return Err(());
            }
            if let Some(list) = body.pointer("/result/songs").and_then(Value::as_array) {
                for s in list {
                    let artist = s["ar"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(" / ");
                    if let (Some(id), Some(name)) = (s["id"].as_u64(), s["name"].as_str()) {
                        out.push(Candidate {
                            id,
                            title: name.into(),
                            artist,
                            seconds: s["dt"].as_f64().unwrap_or_default() / 1000.0,
                            lrc: String::new(),
                        });
                    }
                }
            }
        } else {
            let mut args = vec![("track_name", title)];
            if !artist.is_empty() {
                args.push(("artist_name", artist));
            }
            let body = self.get("https://lrclib.net/api/search", &args)?;
            for s in body.as_array().ok_or(())? {
                if let Some(lrc) = s["syncedLyrics"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Candidate {
                        id: 0,
                        title: json(s, "trackName"),
                        artist: json(s, "artistName"),
                        seconds: s["duration"].as_f64().unwrap_or_default(),
                        lrc: lrc.into(),
                    });
                }
            }
        }
        Ok(out)
    }

    fn fetch(
        &self,
        source: Provider,
        s: &Candidate,
        active: &impl Fn() -> bool,
    ) -> Result<Option<Lyrics>, ()> {
        if source == Provider::LrcLib {
            return Ok(Lyrics::new(&s.title, s.seconds, &s.lrc, "", "LRCLIB"));
        }
        let id = s.id.to_string();
        let params = [
            ("id", id.as_str()),
            ("lv", "-1"),
            ("tv", "-1"),
            ("rv", "-1"),
        ];
        let mut failed = false;
        for url in [
            "https://music.163.com/api/song/lyric",
            "https://music.163.com/api/song/lyric/v1",
        ] {
            if !active() {
                return Ok(None);
            }
            let body = match self.get(url, &params) {
                Ok(b) if b["code"].as_i64() == Some(200) => b,
                _ => {
                    failed = true;
                    continue;
                }
            };
            if body["nolyric"].as_bool() == Some(true) {
                return Ok(None);
            }
            let raw = body
                .pointer("/lrc/lyric")
                .and_then(Value::as_str)
                .unwrap_or("");
            let translated = body
                .pointer("/tlyric/lyric")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(words) = Lyrics::new(&s.title, s.seconds, raw, translated, "网易云曲库")
            {
                return Ok(Some(words));
            }
        }
        if failed { Err(()) } else { Ok(None) }
    }

    pub fn lookup(&self, m: &Media, active: impl Fn() -> bool) -> Outcome {
        let titles = names(&m.song.title, m.song.source.from_video());
        let artist = if m.song.source.from_video() {
            ""
        } else {
            m.song.artist.as_str()
        };
        let order = if m.song.source == Source::NetEase {
            [Provider::NetEase, Provider::LrcLib]
        } else {
            [Provider::LrcLib, Provider::NetEase]
        };
        let mut service_error = false;
        for term in titles.iter().take(3) {
            for source in order {
                if !active() {
                    return Outcome::Missing;
                }
                let candidates = match self.search(source, term, artist) {
                    Ok(c) => c,
                    Err(()) => {
                        service_error = true;
                        continue;
                    }
                };
                let mut ranked = ranked_songs(m, &titles, candidates);
                if ranked.is_empty()
                    && source == Provider::NetEase
                    && !artist.is_empty()
                    && active()
                {
                    match self.search(source, term, "") {
                        Ok(songs) => ranked = ranked_songs(m, &titles, songs),
                        Err(()) => service_error = true,
                    }
                }
                for song in ranked {
                    if !active() {
                        return Outcome::Missing;
                    }
                    match self.fetch(source, &song, &active) {
                        Ok(Some(lyrics)) => return Outcome::Found(lyrics),
                        Err(()) => service_error = true,
                        _ => {}
                    }
                }
            }
        }
        if service_error {
            Outcome::Unavailable
        } else {
            Outcome::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Song;
    #[test]
    fn close_recordings_keep_millisecond_precision_and_real_ties_are_ambiguous() {
        let m = Media {
            song: Song {
                title: "《窗前雨》".into(),
                artist: String::new(),
                source: Source::Bilibili,
            },
            duration: 204.0,
            position: Some(10.0),
            playing: true,
            rate: 1.0,
            sent: 0,
        };
        let titles = names(&m.song.title, true);
        let first = Candidate {
            title: "窗前雨".into(),
            artist: "甲乐团".into(),
            seconds: 204.133,
            id: 1,
            lrc: String::new(),
        };
        let mut second = Candidate {
            artist: "乙乐团".into(),
            seconds: 203.226,
            id: 2,
            ..first.clone()
        };
        assert_eq!(
            ranked_songs(&m, &titles, vec![second.clone(), first.clone()])[0].id,
            1
        );
        second.seconds = 204.140;
        assert!(ranked_songs(&m, &titles, vec![second, first]).is_empty());
    }
    #[test]
    fn browser_title_and_structured_artist_matching() {
        assert_eq!(
            names("【单曲循环】《月下的雨》- 音乐分享_哔哩哔哩_bilibili", true)[0],
            "月下的雨"
        );
        assert_eq!(
            names("歌手 - Moon Rain (Official Music Video) | Bilibili", true)[0],
            "Moon Rain"
        );
        assert_eq!(names("Moon Rain (Live)", false)[0], "Moon Rain (Live)");
        let m = Media {
            song: Song {
                title: "月下的雨".into(),
                artist: "甲".into(),
                source: Source::NetEase,
            },
            duration: 120.0,
            position: None,
            playing: true,
            rate: 1.0,
            sent: 1,
        };
        let mut c = Candidate {
            title: "月下的雨".into(),
            artist: "乙".into(),
            seconds: 120.0,
            id: 0,
            lrc: String::new(),
        };
        let options = names(&m.song.title, false);
        assert!(score(&m, &options, &c).is_none());
        c.artist = "甲".into();
        assert!(score(&m, &options, &c).is_some());
    }
}
