//! Quality filters (hard requirements) and preferences (ranking), modeled on sockseek.

use serde::{Deserialize, Serialize};

use crate::api::{self, SearchFile, SearchResponse};

/// Hard requirements. Missing attributes (peers often omit bit depth etc.) pass unless `strict`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Filter {
    /// Allowed extensions, lowercase, no dot. Empty = anything.
    pub formats: Vec<String>,
    pub min_bitrate: Option<u32>,
    pub max_bitrate: Option<u32>,
    pub min_bitdepth: Option<u32>,
    pub min_samplerate: Option<u32>,
    pub max_samplerate: Option<u32>,
    /// Require the attributes above to be present to pass.
    pub strict: bool,
}

/// Soft preferences used to rank results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Prefs {
    pub formats: Vec<String>,
    pub min_bitdepth: u32,
    pub min_samplerate: u32,
    pub max_samplerate: u32,
    pub min_bitrate: u32,
    pub max_bitrate: u32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            formats: vec!["flac".into()],
            min_bitdepth: 24,
            min_samplerate: 88200,
            max_samplerate: 192000,
            min_bitrate: 320,
            max_bitrate: 10000,
        }
    }
}

const LOSSLESS: &[&str] = &["flac", "wav", "alac", "aiff", "aif", "ape", "wv"];
const LOSSY: &[&str] = &["mp3", "ogg", "m4a", "opus", "aac", "wma"];
/// 1-bit DSD (SACD rips): no bit depth / PCM sample rate limits apply.
const DSD: &[&str] = &["dsf", "dff", "dsd"];
const VIDEO: &[&str] = &["mkv", "mp4", "avi", "m4v", "mov", "webm", "wmv", "ts", "m2ts", "mpg", "mpeg", "flv"];
pub const AUDIO: &[&str] = &["flac", "mp3", "ogg", "m4a", "opus", "wav", "aac", "alac", "aiff", "aif", "ape", "wv", "wma", "dsf", "dff"];

/// Expand a user's type list: extensions (".mkv", "FLAC") and groups
/// (video, dsd, lossless, lossy, audio) → lowercase extensions.
pub fn expand_types<S: AsRef<str>>(items: &[S]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for item in items {
        for t in item.as_ref().split([',', ' ']).map(|t| t.trim().trim_start_matches('.').to_lowercase()).filter(|t| !t.is_empty()) {
            let group: &[&str] = match t.as_str() {
                "video" | "videos" => VIDEO,
                "dsd" | "sacd" => DSD,
                "lossless" => LOSSLESS,
                "lossy" => LOSSY,
                "audio" | "music" => AUDIO,
                _ => &[],
            };
            let add: Vec<String> = if group.is_empty() { vec![t] } else { group.iter().map(|g| g.to_string()).collect() };
            for a in add {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

pub fn ext_of(f: &SearchFile) -> String {
    let e = f.extension.trim_start_matches('.').to_lowercase();
    if !e.is_empty() {
        return e;
    }
    let base = api::basename(&f.filename);
    base.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

pub fn is_audio(f: &SearchFile) -> bool {
    AUDIO.contains(&ext_of(f).as_str())
}

fn check_min(v: Option<u32>, min: Option<u32>, strict: bool) -> bool {
    match (v, min) {
        (_, None) => true,
        (Some(v), Some(m)) => v >= m,
        (None, Some(_)) => !strict,
    }
}

fn check_max(v: Option<u32>, max: Option<u32>, strict: bool) -> bool {
    match (v, max) {
        (_, None) => true,
        (Some(v), Some(m)) => v <= m,
        (None, Some(_)) => !strict,
    }
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        *self == Filter::default()
    }

    pub fn accepts(&self, f: &SearchFile) -> bool {
        if f.is_locked {
            return false;
        }
        let ext = ext_of(f);
        if !self.formats.is_empty() && !self.formats.iter().any(|x| x.eq_ignore_ascii_case(&ext)) {
            return false;
        }
        let lossless = LOSSLESS.contains(&ext.as_str());
        // Bitrate limits only make sense for lossy audio (peers report odd values for FLAC, and
        // video/DSD bitrates aren't comparable).
        let bitrate_ok = !LOSSY.contains(&ext.as_str())
            || (check_min(f.bit_rate, self.min_bitrate, self.strict) && check_max(f.bit_rate, self.max_bitrate, self.strict));
        let depth_ok = !lossless || check_min(f.bit_depth, self.min_bitdepth, self.strict);
        let rate_ok = !lossless
            || (check_min(f.sample_rate, self.min_samplerate, self.strict)
                && check_max(f.sample_rate, self.max_samplerate, self.strict));
        bitrate_ok && depth_ok && rate_ok
    }

    /// One-line description, e.g. "flac ≥16bit ≥44.1kHz".
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "no filter".into();
        }
        // Only mention limits that can affect the chosen types.
        let any = self.formats.is_empty();
        let has = |group: &[&str]| any || self.formats.iter().any(|f| group.contains(&f.as_str()));
        let (lossy, lossless) = (has(LOSSY), has(LOSSLESS));
        let mut parts = vec![];
        if !self.formats.is_empty() {
            parts.push(self.formats.join("/"));
        }
        if lossy {
            if let Some(b) = self.min_bitrate {
                parts.push(format!("≥{b}kbps"));
            }
            if let Some(b) = self.max_bitrate {
                parts.push(format!("≤{b}kbps"));
            }
        }
        if lossless {
            if let Some(d) = self.min_bitdepth {
                parts.push(format!("≥{d}bit"));
            }
            if let Some(r) = self.min_samplerate {
                parts.push(format!("≥{}kHz", khz(r)));
            }
            if let Some(r) = self.max_samplerate {
                parts.push(format!("≤{}kHz", khz(r)));
            }
        }
        if self.strict {
            parts.push("strict".into());
        }
        if parts.is_empty() { "no filter".into() } else { parts.join(" ") }
    }
}

pub fn khz(r: u32) -> String {
    let k = r as f64 / 1000.0;
    if (k - k.round()).abs() < 0.05 {
        format!("{}", k.round() as u32)
    } else {
        format!("{k:.1}")
    }
}

/// "FLAC 24/96", "MP3 320", "MP3 V0~245".
pub fn quality_label(f: &SearchFile) -> String {
    let ext = ext_of(f);
    let up = ext.to_uppercase();
    if DSD.contains(&ext.as_str()) {
        // DSD64 = 64 × 44.1 kHz.
        return match f.sample_rate {
            Some(r) if r >= 2_822_400 => format!("{up} DSD{}", r / 44_100),
            _ => format!("{up} DSD"),
        };
    }
    if VIDEO.contains(&ext.as_str()) {
        return up;
    }
    if LOSSLESS.contains(&ext.as_str()) {
        match (f.bit_depth, f.sample_rate) {
            (Some(d), Some(r)) => format!("{up} {d}/{}", khz(r)),
            (None, Some(r)) => format!("{up} {}k", khz(r)),
            (Some(d), None) => format!("{up} {d}bit"),
            _ => up,
        }
    } else {
        match f.bit_rate {
            Some(b) if f.is_variable_bit_rate == Some(true) => format!("{up} ~{b}"),
            Some(b) => format!("{up} {b}"),
            None => up,
        }
    }
}

impl Prefs {
    /// Quality score of the file itself (0..~100).
    pub fn file_score(&self, f: &SearchFile) -> f64 {
        let ext = ext_of(f);
        let mut s = 0.0;
        if let Some(pos) = self.formats.iter().position(|x| x.eq_ignore_ascii_case(&ext)) {
            s += 40.0 - pos as f64 * 5.0;
        } else if LOSSLESS.contains(&ext.as_str()) || DSD.contains(&ext.as_str()) {
            s += 25.0;
        }
        if LOSSLESS.contains(&ext.as_str()) {
            if let Some(d) = f.bit_depth {
                if d >= self.min_bitdepth {
                    s += 15.0;
                } else if d >= 16 {
                    s += 8.0;
                }
            } else {
                s += 6.0;
            }
            if let Some(r) = f.sample_rate {
                if r > self.max_samplerate {
                    s += 2.0; // absurdly hi-res files are huge; don't prefer them
                } else if r >= self.min_samplerate {
                    s += 15.0;
                } else if r >= 44100 {
                    s += 8.0;
                }
            } else {
                s += 6.0;
            }
        } else if let Some(b) = f.bit_rate {
            if b >= self.min_bitrate && b <= self.max_bitrate {
                s += 20.0;
            } else {
                s += (b as f64 / self.min_bitrate.max(1) as f64 * 20.0).min(20.0);
            }
        }
        s
    }
}

/// Score how likely the peer is to actually deliver quickly (0..~30).
pub fn peer_score(r: &SearchResponse) -> f64 {
    let mut s = 0.0;
    if r.has_free_upload_slot {
        s += 15.0;
    }
    s += 10.0 / (1.0 + r.queue_length as f64 / 5.0);
    // speed in bytes/sec; saturate around 5 MB/s
    s += (r.upload_speed as f64 / 5_000_000.0).min(1.0) * 8.0;
    s
}

/// A flattened search result: one file from one peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub username: String,
    pub file: SearchFile,
    pub free_slot: bool,
    pub queue_length: u64,
    pub upload_speed: u64,
    pub score: f64,
}

impl Hit {
    pub fn dir(&self) -> &str {
        api::dirname(&self.file.filename)
    }
    pub fn name(&self) -> &str {
        api::basename(&self.file.filename)
    }
}

/// Flatten responses into scored hits that pass `filter`. Returns (hits, hidden_count).
pub fn rank(responses: &[SearchResponse], filter: &Filter, prefs: &Prefs) -> (Vec<Hit>, usize) {
    let mut hits = vec![];
    let mut hidden = 0;
    for r in responses {
        let ps = peer_score(r);
        for f in &r.files {
            if !filter.accepts(f) {
                hidden += 1;
                continue;
            }
            hits.push(Hit {
                username: r.username.clone(),
                file: f.clone(),
                free_slot: r.has_free_upload_slot,
                queue_length: r.queue_length,
                upload_speed: r.upload_speed,
                score: prefs.file_score(f) + ps,
            });
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    (hits, hidden)
}

/// Like `rank`, but also rewards files whose *name* (not just folder) matches the query words,
/// so "take five" puts "Take Five.flac" above other tracks from a folder called "Take Five".
pub fn rank_query(responses: &[SearchResponse], filter: &Filter, prefs: &Prefs, query: &str) -> (Vec<Hit>, usize) {
    let (mut hits, hidden) = rank(responses, filter, prefs);
    let words: Vec<String> = query
        .split_whitespace()
        .filter(|w| !w.starts_with('-'))
        .map(|w| w.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect();
    if !words.is_empty() {
        for h in &mut hits {
            let name = h.name().to_lowercase();
            let n = words.iter().filter(|w| name.contains(w.as_str())).count();
            h.score += 30.0 * n as f64 / words.len() as f64;
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    }
    (hits, hidden)
}

/// A folder (album) grouping of hits from one user.
#[derive(Debug, Clone)]
pub struct Folder {
    pub username: String,
    pub dir: String,
    pub files: Vec<Hit>,
    pub score: f64,
}

impl Folder {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|h| h.file.size).sum()
    }
    pub fn audio_count(&self) -> usize {
        self.files.iter().filter(|h| is_audio(&h.file)).count()
    }
}

pub fn group_folders(hits: &[Hit]) -> Vec<Folder> {
    use std::collections::HashMap;
    let mut map: HashMap<(String, String), Vec<Hit>> = HashMap::new();
    for h in hits {
        map.entry((h.username.clone(), h.dir().to_string())).or_default().push(h.clone());
    }
    let mut folders: Vec<Folder> = map
        .into_iter()
        .map(|((username, dir), mut files)| {
            files.sort_by(|a, b| a.file.filename.cmp(&b.file.filename));
            let avg = files.iter().map(|h| h.score).sum::<f64>() / files.len() as f64;
            // Mildly reward complete-looking folders.
            let score = avg + (files.len() as f64).ln() * 2.0;
            Folder { username, dir, files, score }
        })
        .collect();
    folders.sort_by(|a, b| b.score.total_cmp(&a.score));
    folders
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, depth: Option<u32>, rate: Option<u32>, bitrate: Option<u32>) -> SearchFile {
        SearchFile { filename: name.into(), bit_depth: depth, sample_rate: rate, bit_rate: bitrate, ..Default::default() }
    }

    #[test]
    fn type_groups() {
        assert_eq!(expand_types(&[".MKV"]), vec!["mkv"]);
        assert_eq!(expand_types(&["dsd"]), vec!["dsf", "dff", "dsd"]);
        assert!(expand_types(&["video, flac"]).contains(&"mp4".to_string()));
        assert!(expand_types(&["video, flac"]).contains(&"flac".to_string()));
    }

    #[test]
    fn dsd_and_video_pass_audio_limits() {
        // Lossless-style limits (≥16 bit, ≥44.1k, ≥192kbps) must not reject DSD or video.
        let f = Filter {
            formats: expand_types(&["dsd", "video"]),
            min_bitdepth: Some(16),
            min_samplerate: Some(44100),
            min_bitrate: Some(192),
            ..Default::default()
        };
        assert!(f.accepts(&file("a\\01.dsf", Some(1), Some(2_822_400), None)));
        assert!(f.accepts(&file("show\\S01E01.mkv", None, None, Some(128))));
        assert!(!f.accepts(&file("a\\01.flac", Some(24), Some(96000), None)));
        assert_eq!(quality_label(&file("a\\01.dsf", Some(1), Some(5_644_800), None)), "DSF DSD128");
    }
}
