//! Synced lyrics from LRCLIB (free, no API key). Fetched once per track
//! on a background thread; the UI picks the line for the playback position.

use crate::config;
use crate::net;
use crate::worker::Shared;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Lyrics {
    pub track_id: u64,
    /// (start seconds, text), sorted.
    pub lines: Vec<(f64, String)>,
}

impl Lyrics {
    /// The line being sung at `pos` seconds (empty between verses).
    pub fn line_at(&self, pos: f64) -> &str {
        let i = self.lines.partition_point(|(t, _)| *t <= pos + 0.25);
        if i == 0 {
            return "";
        }
        &self.lines[i - 1].1
    }
}

pub fn spawn(shared: Arc<Mutex<Shared>>) {
    std::thread::Builder::new()
        .name("isla-lyrics".into())
        .spawn(move || {
            let mut last = 0u64;
            loop {
                std::thread::sleep(Duration::from_millis(700));
                if !config::current().features.lyrics {
                    shared.lock().unwrap().lyrics = None;
                    last = 0;
                    continue;
                }
                let track = shared
                    .lock()
                    .unwrap()
                    .media
                    .as_ref()
                    .map(|m| (m.track_id, m.title.clone(), m.artist.clone(), m.duration));
                let Some((id, title, artist, duration)) = track else { continue };
                if id == last {
                    continue;
                }
                last = id;
                shared.lock().unwrap().lyrics = None;
                if let Some(lines) = fetch(&title, &artist, duration) {
                    let mut sh = shared.lock().unwrap();
                    // Only publish if the track did not change meanwhile.
                    if sh.media.as_ref().is_some_and(|m| m.track_id == id) {
                        sh.lyrics = Some(Arc::new(Lyrics { track_id: id, lines }));
                    }
                }
            }
        })
        .expect("spawn lyrics");
}

/// Strips "(Official Video)", "[Lyrics]", "ft. X" and splits "Artist - Title"
/// (common for YouTube) into a cleaner query.
fn clean(title: &str, artist: &str) -> (String, String) {
    let mut t = title.to_string();
    for (open, close) in [('(', ')'), ('[', ']'), ('【', '】')] {
        while let (Some(a), Some(b)) = (t.find(open), t.find(close)) {
            if b <= a {
                break;
            }
            t.replace_range(a..b + close.len_utf8(), "");
        }
    }
    let mut a = artist.trim().trim_end_matches(" - Topic").to_string();
    if let Some((left, right)) = t.split_once(" - ") {
        if a.is_empty() || left.to_lowercase().contains(&a.to_lowercase()) || a.to_lowercase().contains("vevo") {
            a = left.trim().to_string();
        }
        t = right.to_string();
    }
    for sep in [" ft.", " feat.", " ft ", " | "] {
        if let Some(i) = t.to_lowercase().find(sep) {
            t.truncate(i);
        }
    }
    (t.trim().to_string(), a)
}

fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            if let Some((m, s)) = tag.split_once(':') {
                if let (Ok(m), Ok(s)) = (m.parse::<f64>(), s.parse::<f64>()) {
                    stamps.push(m * 60.0 + s);
                }
            }
            rest = &stripped[end + 1..];
        }
        for t in stamps {
            out.push((t, rest.trim().to_string()));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn fetch(title: &str, artist: &str, duration: f64) -> Option<Vec<(f64, String)>> {
    let (t, a) = clean(title, artist);
    if t.is_empty() {
        return None;
    }
    let synced = |v: &Value| v["syncedLyrics"].as_str().map(parse_lrc).filter(|l| !l.is_empty());
    let mut url = format!(
        "https://lrclib.net/api/get?track_name={}&artist_name={}",
        net::encode(&t),
        net::encode(&a)
    );
    if duration > 0.0 {
        url.push_str(&format!("&duration={}", duration.round() as u64));
    }
    if let Some(v) = net::get(&url).and_then(|b| serde_json::from_str::<Value>(&b).ok()) {
        if let Some(l) = synced(&v) {
            return Some(l);
        }
    }
    let url = format!("https://lrclib.net/api/search?q={}", net::encode(&format!("{a} {t}")));
    let v: Value = serde_json::from_str(&net::get(&url)?).ok()?;
    v.as_array()?.iter().find_map(synced)
}
