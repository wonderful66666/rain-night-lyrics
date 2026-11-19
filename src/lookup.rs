use crate::media::{Media, Song, Source};
use crate::protocol::Lyrics;
use regex::{Regex, RegexSet};
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
static EDITIONS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)(?-u:\blive\b)|现场|現場",
        r"(?i)(?-u:\b(remix|rework|bootleg)\b)|混音",
        r"(?i)(?-u:\bcover\b)|翻唱",
        r"(?i)(?-u:\b(instrumental|karaoke|off vocal)\b)|伴奏|纯音乐|純音樂",
        r"(?i)(?-u:\b(acoustic|unplugged)\b)|不插电|不插電",
        r"(?i)(?-u:\b(sped up|nightcore)\b)|加速",
        r"(?i)(?-u:\bslowed\b)|慢速|降速",
        r"(?i)(?-u:\bremaster(?:ed)?\b)|重制|重製",
        r"(?i)(?-u:\bdemo\b)|样带|樣帶",
        r"(?i)(?-u:\bedit\b)|剪辑版|剪輯版",
    ])
    .unwrap()
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

pub(crate) fn suggested_title(title: &str, source: Source) -> String {
    names(title, source.from_video())
        .into_iter()
        .find(|s| crate::media::eligible_title(s))
        .unwrap_or_default()
}

fn query_title(song: &Song) -> Option<&str> {
    crate::media::eligible_title(&song.title).then_some(song.title.as_str())
}

#[derive(Clone)]
struct Candidate {
    provider: Provider,
    title: String,
    artist: String,
    album_editions: u16,
    seconds: f64,
    id: u64,
    lrc: String,
}

fn editions(text: &str) -> u16 {
    EDITIONS
        .matches(text)
        .iter()
        .fold(0, |flags, i| flags | 1 << i)
}

fn artist_key(name: &str) -> String {
    let mut names: Vec<_> = name
        .split(['/', '&', '＆', '、', ';', '；', ','])
        .map(key)
        .filter(|n| !n.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names.join("/")
}

fn enough_catalogs(finished: bool, responses: usize, local_artist: &str) -> bool {
    finished && (responses == 2 || (responses == 1 && !artist_key(local_artist).is_empty()))
}

fn track_key(title: &str) -> String {
    let normalized: String = title
        .chars()
        .map(|c| match c {
            '！'..='～' => char::from_u32(c as u32 - 0xfee0).unwrap(),
            '‐' | '‑' | '–' | '—' => '-',
            '‘' | '’' => '\'',
            _ => c,
        })
        .flat_map(char::to_lowercase)
        .collect();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Match<'a> {
    song: &'a Candidate,
    artist: String,
    confirmed_artist: bool,
    duration_error: Option<f64>,
}

fn assess<'a>(
    m: &Media,
    title: &str,
    local_artist: &str,
    version: u16,
    song: &'a Candidate,
) -> Option<Match<'a>> {
    let artist = artist_key(&song.artist);
    if title != track_key(&song.title)
        || artist.is_empty()
        || version != (editions(&song.title) | song.album_editions)
    {
        return None;
    }
    let confirmed_artist = artist == local_artist;
    if !confirmed_artist && (!local_artist.is_empty() || key(title).chars().count() < 3) {
        return None;
    }
    let mut error = None;
    if m.duration.is_finite() && m.duration > 0.0 && song.seconds.is_finite() && song.seconds > 0.0
    {
        let delta = (song.seconds - m.duration).abs();
        if delta > 5.0 {
            return None;
        }
        error = Some(delta);
    }
    Some(Match {
        song,
        artist,
        confirmed_artist,
        duration_error: error,
    })
}

fn ranked_songs<'a>(m: &Media, candidates: &'a [Candidate], complete: bool) -> Vec<&'a Candidate> {
    let timed = m.duration.is_finite() && m.duration > 0.0;
    if !timed && !complete {
        return vec![];
    }
    let title = track_key(&m.song.title);
    let artist = artist_key(&m.song.artist);
    let version = editions(&m.song.title);
    let mut matches: Vec<_> = candidates
        .iter()
        .filter_map(|s| assess(m, &title, &artist, version, s))
        .collect();
    let confirmed = matches.iter().any(|v| v.confirmed_artist);
    if confirmed {
        matches.retain(|v| v.confirmed_artist);
    } else if !complete {
        return vec![];
    }
    matches.sort_by(|a, b| {
        a.duration_error
            .unwrap_or(f64::INFINITY)
            .total_cmp(&b.duration_error.unwrap_or(f64::INFINITY))
    });
    let Some(best) = matches.first() else {
        return vec![];
    };
    if !confirmed && best.duration_error.is_none_or(|delta| delta > 1.5) {
        return vec![];
    }
    if matches.iter().enumerate().any(|(index, v)| {
        v.artist != best.artist
            || (v.song.seconds - best.song.seconds).abs() > if timed { 3.0 } else { 1.0 }
            || (!timed
                && matches.len() > 1
                && (v.song.seconds <= 0.0
                    || matches[..index].iter().any(|other| {
                        other.song.provider == v.song.provider && other.song.id != v.song.id
                    })))
    }) {
        return vec![];
    }
    matches
        .into_iter()
        .filter(|v| confirmed || v.duration_error.is_some_and(|d| d <= 1.5))
        .map(|v| v.song)
        .take(4)
        .collect()
}

fn has_related_title(m: &Media, candidates: &[Candidate]) -> bool {
    let hints = names(&m.song.title, m.song.source.from_video());
    candidates.iter().any(|s| {
        let title = key(&s.title);
        hints.iter().any(|n| key(n) == title)
    })
}

fn lyric_identity_matches(raw: &str, song: &Candidate) -> bool {
    raw.lines().take(40).all(|line| {
        let line = line.trim().trim_start_matches('\u{feff}');
        let Some((tag, value)) = line
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .and_then(|s| s.split_once(':'))
        else {
            return true;
        };
        let value = value.trim();
        if value.is_empty() {
            return true;
        }
        if tag.eq_ignore_ascii_case("ti") {
            return key(value) == key(&song.title);
        }
        if tag.eq_ignore_ascii_case("ar") {
            return artist_key(value) == artist_key(&song.artist);
        }
        true
    })
}

fn json(s: &Value, name: &str) -> String {
    s[name].as_str().unwrap_or("").to_owned()
}

fn netease_recordings(body: &Value) -> Result<Vec<Candidate>, ()> {
    if body["code"].as_i64() != Some(200) {
        return Err(());
    }
    let result = body["result"].as_object().ok_or(())?;
    let count = result
        .get("songCount")
        .map(|v| v.as_u64().ok_or(()))
        .transpose()?;
    let rows = match result.get("songs") {
        None | Some(Value::Null) if count == Some(0) => return Ok(vec![]),
        Some(Value::Array(rows)) if rows.is_empty() && count.is_some_and(|n| n > 0) => {
            return Err(());
        }
        Some(Value::Array(rows)) => rows,
        _ => return Err(()),
    };
    let mut out = Vec::new();
    for row in rows.iter().take(12) {
        let id = row["id"].as_u64().filter(|id| *id > 0).ok_or(())?;
        let title = row["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .ok_or(())?;
        let mut artists = ["ar", "artists"]
            .iter()
            .filter_map(|field| row[*field].as_array());
        let credits = artists.find(|items| !items.is_empty()).ok_or(())?;
        let names: Option<Vec<_>> = credits
            .iter()
            .map(|a| a["name"].as_str().filter(|name| !name.trim().is_empty()))
            .collect();
        let artist = names.ok_or(())?.join(" / ");
        let duration = |field: &str| {
            row[field]
                .as_f64()
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
        };
        let primary = duration("dt");
        let seconds = primary
            .filter(|ms| *ms > 0.0)
            .or_else(|| duration("duration"))
            .or(primary)
            .ok_or(())?
            / 1000.0;
        let album = ["/al/name", "/album/name"]
            .iter()
            .filter_map(|path| row.pointer(path)?.as_str())
            .find(|name| !name.is_empty())
            .unwrap_or("");
        out.push(Candidate {
            provider: Provider::NetEase,
            id,
            title: title.into(),
            artist,
            seconds,
            album_editions: editions(album),
            lrc: String::new(),
        });
    }
    Ok(out)
}

#[derive(Debug)]
pub enum Outcome {
    Found(Lyrics),
    Missing,
    Ambiguous,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    NetEase,
    LrcLib,
}

pub struct Catalog(ureq::Agent);
impl Catalog {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(5)))
            .user_agent(concat!("RainNightLyrics/", env!("CARGO_PKG_VERSION")))
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
        let mut response = request.call().map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_json()
            .map_err(|_| ())
    }

    fn search(&self, provider: Provider, title: &str) -> Result<Vec<Candidate>, ()> {
        let mut out = Vec::new();
        if provider == Provider::NetEase {
            let body = self.get(
                "https://music.163.com/api/cloudsearch/pc",
                &[
                    ("s", title),
                    ("type", "1"),
                    ("limit", "12"),
                    ("offset", "0"),
                ],
            )?;
            return netease_recordings(&body);
        } else {
            let body = self.get("https://lrclib.net/api/search", &[("track_name", title)])?;
            for s in body.as_array().ok_or(())?.iter().take(50) {
                if let Some(id) = s["id"].as_u64() {
                    out.push(Candidate {
                        provider,
                        id,
                        title: json(s, "trackName"),
                        artist: json(s, "artistName"),
                        album_editions: editions(s["albumName"].as_str().unwrap_or("")),
                        seconds: s["duration"].as_f64().unwrap_or_default(),
                        lrc: json(s, "syncedLyrics"),
                    });
                }
            }
        }
        Ok(out)
    }

    fn fetch(&self, s: &Candidate, active: &impl Fn() -> bool) -> Outcome {
        if s.provider == Provider::LrcLib {
            if !lyric_identity_matches(&s.lrc, s) {
                return Outcome::Ambiguous;
            }
            return Lyrics::new(&s.title, s.seconds, &s.lrc, "", "LRCLIB")
                .map_or(Outcome::Missing, Outcome::Found);
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
                return Outcome::Missing;
            }
            let body = match self.get(url, &params) {
                Ok(b) if b["code"].as_i64() == Some(200) => b,
                _ => {
                    failed = true;
                    continue;
                }
            };
            if body["nolyric"].as_bool() == Some(true) {
                return Outcome::Missing;
            }
            let raw = body
                .pointer("/lrc/lyric")
                .and_then(Value::as_str)
                .unwrap_or("");
            let translated = body
                .pointer("/tlyric/lyric")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !lyric_identity_matches(raw, s) {
                return Outcome::Ambiguous;
            }
            if let Some(words) = Lyrics::new(
                &s.title,
                s.seconds,
                raw,
                if lyric_identity_matches(translated, s) {
                    translated
                } else {
                    ""
                },
                "网易云曲库",
            ) {
                return Outcome::Found(words);
            }
        }
        if failed {
            Outcome::Unavailable
        } else {
            Outcome::Missing
        }
    }

    pub fn lookup(&self, m: &Media, active: impl Fn() -> bool) -> Outcome {
        let Some(title) = query_title(&m.song) else {
            return Outcome::Missing;
        };
        let order = if m.song.source == Source::MusicApp {
            [Provider::NetEase, Provider::LrcLib]
        } else {
            [Provider::LrcLib, Provider::NetEase]
        };
        let mut service_error = false;
        let mut conflicting_lyrics = false;
        let mut candidates = Vec::new();
        let mut searched = 0;
        let mut fetched = Vec::new();
        for (i, source) in order.into_iter().enumerate() {
            if !active() {
                return Outcome::Missing;
            }
            match self.search(source, title) {
                Ok(items) => {
                    candidates.extend(items);
                    searched += 1;
                }
                Err(()) => {
                    service_error = true;
                }
            }
            for song in ranked_songs(
                m,
                &candidates,
                enough_catalogs(i == 1, searched, &m.song.artist),
            ) {
                if !active() {
                    return Outcome::Missing;
                }
                if fetched.contains(&(song.provider, song.id)) {
                    continue;
                }
                fetched.push((song.provider, song.id));
                match self.fetch(song, &active) {
                    Outcome::Found(lyrics) => return Outcome::Found(lyrics),
                    Outcome::Unavailable => service_error = true,
                    Outcome::Ambiguous => conflicting_lyrics = true,
                    Outcome::Missing => {}
                }
            }
        }
        if service_error {
            Outcome::Unavailable
        } else if conflicting_lyrics || (fetched.is_empty() && has_related_title(m, &candidates)) {
            Outcome::Ambiguous
        } else {
            Outcome::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_supported_sources_search_only_the_exact_approved_title() {
        let mut song = Song {
            title: "【单曲循环】《窗前雨》 | 歌单".into(),
            artist: "UP 主".into(),
            source: Source::Browser,
        };
        for source in [Source::Browser, Source::MusicApp, Source::Other] {
            song.source = source;
            assert_eq!(query_title(&song), Some(song.title.as_str()));
        }
        song.source = Source::MusicApp;
        song.title = " ".into();
        assert!(query_title(&song).is_none());
    }
    fn request(title: &str, artist: &str, duration: f64) -> Media {
        Media {
            song: Song {
                title: title.into(),
                artist: artist.into(),
                source: Source::Browser,
            },
            duration,
            position: Some(0.0),
            playing: true,
            rate: 1.0,
            sent: 0,
        }
    }

    fn recording(
        provider: Provider,
        id: u64,
        title: &str,
        artist: &str,
        seconds: f64,
    ) -> Candidate {
        Candidate {
            provider,
            id,
            title: title.into(),
            artist: artist.into(),
            album_editions: 0,
            seconds,
            lrc: String::new(),
        }
    }

    fn catalog_recordings() -> Vec<Candidate> {
        use Provider::{LrcLib, NetEase};
        vec![
            recording(LrcLib, 1, "Silver Meadow", "Amber Choir", 55.1),
            recording(LrcLib, 2, "Silver Meadow", "Amber Choir", 65.0),
            recording(LrcLib, 3, "Silver Meadow", "Amber Choir", 71.0),
            recording(NetEase, 4, "Silver Meadow", "松林乐团", 121.6),
            recording(NetEase, 5, "Silver Meadow", "北岸乐团", 157.9),
            recording(NetEase, 6, "Silver Meadow (Edit)", "Nerai", 207.3),
            recording(NetEase, 7, "Silver Meadow", "Nerai", 141.0),
        ]
    }

    #[test]
    fn joins_catalogs_before_resolving_a_title_without_an_artist() {
        let mut songs = catalog_recordings();
        let m = request("Silver Meadow", "", 141.15);
        assert!(ranked_songs(&m, &songs[..3], false).is_empty());
        assert!(ranked_songs(&m, &songs, false).is_empty());
        assert_eq!(ranked_songs(&m, &songs, true)[0].id, 7);
        assert!(ranked_songs(&request("Silver Meadow", "", 0.0), &songs, true).is_empty());
        songs.push(recording(
            Provider::LrcLib,
            9,
            "Silver Meadow",
            "不同乐团",
            140.25,
        ));
        assert!(ranked_songs(&m, &songs, true).is_empty());
        assert_eq!(
            ranked_songs(&request("Silver Meadow", "Nerai", 141.0), &songs, true)[0].id,
            7
        );
        assert!(ranked_songs(&request("Silver Meadow", "Neira", 141.0), &songs, true).is_empty());
    }

    #[test]
    fn approved_song_title_is_literal_and_local_artist_selects_matching_editions() {
        let songs = catalog_recordings();
        for title in [
            "Nerai - Silver Meadow",
            "Silver Meadow – Neira",
            "Neira《Silver Meadow》",
            "Ne - Silver Meadow",
            "Random video | Silver Meadow",
            "【翻唱】《Silver Meadow》",
        ] {
            assert!(ranked_songs(&request(title, "", 141.0), &songs, true).is_empty());
        }
        let wrapped = request(
            "【单曲循环】《Silver Meadow》- 音乐分享_哔哩哔哩_bilibili",
            "",
            141.0,
        );
        assert!(ranked_songs(&wrapped, &songs, true).is_empty());
        let typed = request("sILVER MEADOW", "Nerai", 0.0);
        assert!(ranked_songs(&typed, &songs, false).is_empty());
        assert_eq!(ranked_songs(&typed, &songs, true)[0].id, 7);
        assert!(ranked_songs(&request("Silver Meadow", "Nerai", 207.3), &songs, true).is_empty());
        assert_eq!(
            ranked_songs(
                &request("Silver Meadow (Edit)", "Nerai", 207.3),
                &songs,
                true
            )[0]
            .id,
            6
        );
        assert!(
            ranked_songs(
                &request("Silver Meadow (Live)", "Nerai", 141.0),
                &songs,
                true
            )
            .is_empty()
        );
        assert!(ranked_songs(&request("Silver Meadow", "Nerai", 65.0), &songs, true).is_empty());
    }

    #[test]
    fn title_with_a_dash_and_manual_artist_cannot_be_reinterpreted_as_a_different_song() {
        let m = request("Day - Break", "North", 0.0);
        let mut songs = vec![
            recording(Provider::NetEase, 1, "Day", "Break", 180.0),
            recording(Provider::LrcLib, 2, "Day Break", "North", 180.0),
            recording(Provider::NetEase, 3, "Day – Break", "North", 180.0),
        ];
        assert_eq!(ranked_songs(&m, &songs, true)[0].id, 3);
        songs.push(recording(
            Provider::LrcLib,
            4,
            "Day - Break",
            "North",
            179.8,
        ));
        assert_eq!(ranked_songs(&m, &songs, true).len(), 2);
        songs.push(recording(
            Provider::LrcLib,
            5,
            "Day - Break",
            "North",
            180.1,
        ));
        assert!(ranked_songs(&m, &songs, true).is_empty());
        songs.pop();
        songs[3].seconds = 250.0;
        assert!(ranked_songs(&m, &songs, true).is_empty());
    }

    #[test]
    fn temporary_provider_failure_allows_only_a_confirmed_artist_after_both_attempts() {
        let songs = catalog_recordings();
        let selection = request("Silver Meadow", "Nerai", 0.0);
        assert!(ranked_songs(&selection, &songs, enough_catalogs(false, 1, "Nerai")).is_empty());
        assert_eq!(
            ranked_songs(&selection, &songs, enough_catalogs(true, 1, "Nerai"))[0].id,
            7
        );
        let name_only = request("Silver Meadow", "", 141.1);
        assert!(ranked_songs(&name_only, &songs, enough_catalogs(true, 1, "")).is_empty());
        assert!(!enough_catalogs(true, 0, "Nerai"));
        assert!(!enough_catalogs(true, 1, " -- "));
    }

    #[test]
    fn netease_legacy_and_modern_schemas_retain_the_same_recording_metadata() {
        use serde_json::json;
        let current = json!({"code":200,"result":{"songs":[{"id":19,"name":"River",
            "ar":[{"name":"North"}],"al":{"name":"Live at Dawn"},"dt":141250}]}});
        let legacy = json!({"code":200,"result":{"songs":[{"id":19,"name":"River",
            "artists":[{"name":"North"}],"album":{"name":"Live at Dawn"},"duration":141250}]}});
        for value in [&current, &legacy] {
            let records = netease_recordings(value).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].artist, "North");
            assert_eq!(records[0].seconds, 141.25);
            assert_eq!(records[0].album_editions, editions("Live"));
        }
    }

    #[test]
    fn netease_empty_and_malformed_results_are_distinct() {
        use serde_json::json;
        for result in [
            json!({"songs":[]}),
            json!({"songCount":0}),
            json!({"songCount":0,"songs":null}),
        ] {
            assert!(
                netease_recordings(&json!({"code":200,"result":result}))
                    .unwrap()
                    .is_empty()
            );
        }
        for result in [
            Value::Null,
            json!({}),
            json!({"songs":"unexpected"}),
            json!({"songCount":1,"songs":[]}),
            json!({"songs":[{"id":1,"name":"River"}]}),
        ] {
            assert!(netease_recordings(&json!({"code":200,"result":result})).is_err());
        }
        assert!(netease_recordings(&json!({"code":429})).is_err());
    }

    #[test]
    fn short_titles_conflicting_lyric_headers_and_hidden_editions_do_not_match() {
        let mut song = recording(Provider::NetEase, 1, "雨", "乐团", 120.0);
        assert!(
            ranked_songs(&request("雨", "", 120.0), std::slice::from_ref(&song), true).is_empty()
        );
        assert!(lyric_identity_matches(
            "[ti:雨]\n[ar:乐团]\n[00:20]雨",
            &song
        ));
        assert!(!lyric_identity_matches("[ti:另一首]\n[00:20]雨", &song));
        assert!(!lyric_identity_matches("[ar:大乐团]\n[00:20]雨", &song));
        song.album_editions = editions("Live at the Arena");
        assert!(ranked_songs(&request("雨", "乐团", 120.0), &[song], true).is_empty());
    }

    #[test]
    fn confirmed_recording_without_lyrics_is_distinct_from_mislabeled_lyrics() {
        let catalog = Catalog::new();
        let mut song = recording(Provider::LrcLib, 1, "雨", "乐团", 120.0);
        assert!(matches!(catalog.fetch(&song, &|| true), Outcome::Missing));
        song.lrc = "[ti:另一首]\n[00:00]雨".into();
        assert!(matches!(catalog.fetch(&song, &|| true), Outcome::Ambiguous));
        song.lrc = "[ti:雨]\n[00:00]雨".into();
        assert!(matches!(catalog.fetch(&song, &|| true), Outcome::Found(_)));
    }
}
