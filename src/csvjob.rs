//! Batch downloading from a CSV (Spotify exports, sockseek-style lists).
//!
//! Rows with a title are track downloads; rows with only an album are album downloads.
//! Progress is saved per CSV so a run can be interrupted and resumed.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::CsvArgs;
use crate::api::{self, Client, SearchFile, SearchResponse};
use crate::config::{self, Config};
use crate::download::{self, Target};
use crate::quality::{self, Filter, Hit};

// ---------- CSV parsing ----------

#[derive(Debug, Clone)]
pub struct Row {
    pub line: usize,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub length: Option<u32>,
}

impl Row {
    pub fn is_album(&self) -> bool {
        self.title.trim().is_empty()
    }
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}",
            norm(&self.artist),
            norm(&self.title),
            norm(&self.album)
        )
    }
    pub fn label(&self) -> String {
        let what = if self.is_album() {
            &self.album
        } else {
            &self.title
        };
        if self.artist.is_empty() {
            what.clone()
        } else {
            format!("{} - {what}", self.artist)
        }
    }
}

const TITLE_COLS: &[&str] = &[
    "title",
    "track",
    "track name",
    "trackname",
    "track title",
    "song",
    "song name",
    "name",
];
const ARTIST_COLS: &[&str] = &[
    "artist",
    "artists",
    "artist name",
    "artist name(s)",
    "artist(s)",
    "artist names",
    "performer",
    "album artist",
];
const ALBUM_COLS: &[&str] = &["album", "album name", "album title", "release"];
const LENGTH_COLS: &[&str] = &[
    "duration_ms",
    "duration (ms)",
    "track duration (ms)",
    "length",
    "duration",
    "time",
    "length (s)",
];

fn find_col(
    headers: &[String],
    explicit: Option<&str>,
    candidates: &[&str],
) -> Result<Option<usize>> {
    let norm_h: Vec<String> = headers
        .iter()
        .map(|h| h.trim().to_lowercase().replace('_', " "))
        .collect();
    if let Some(e) = explicit {
        let e = e.trim().to_lowercase().replace('_', " ");
        return norm_h
            .iter()
            .position(|h| *h == e)
            .map(Some)
            .with_context(|| format!("column '{e}' not found in CSV header"));
    }
    for c in candidates {
        let c = c.replace('_', " ");
        if let Some(i) = norm_h.iter().position(|h| *h == c) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// "3:47", "227333" (ms), "227" (s), "1:02:03".
fn parse_length(raw: &str, header: &str) -> Option<u32> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        let mut secs = 0u32;
        for part in s.split(':') {
            secs = secs * 60 + part.trim().parse::<f64>().ok()? as u32;
        }
        return Some(secs);
    }
    let v: f64 = s.parse().ok()?;
    let h = header.to_lowercase();
    if h.contains("ms") || v > 36_000.0 {
        Some((v / 1000.0).round() as u32)
    } else {
        Some(v.round() as u32)
    }
}

pub fn read_rows(path: &Path, args: &CsvArgs) -> Result<Vec<Row>> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_path(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let headers: Vec<String> = rdr.headers()?.iter().map(str::to_string).collect();
    let title = find_col(&headers, args.title_col.as_deref(), TITLE_COLS)?;
    let artist = find_col(&headers, args.artist_col.as_deref(), ARTIST_COLS)?;
    let album = find_col(&headers, args.album_col.as_deref(), ALBUM_COLS)?;
    let length = find_col(&headers, args.length_col.as_deref(), LENGTH_COLS)?;
    if title.is_none() && album.is_none() {
        bail!(
            "couldn't find a title or album column (headers: {}); use --title-col / --album-col",
            headers.join(", ")
        );
    }
    let get = |rec: &csv::StringRecord, i: Option<usize>| {
        i.and_then(|i| rec.get(i)).unwrap_or("").trim().to_string()
    };
    let mut rows = vec![];
    for (i, rec) in rdr.records().enumerate() {
        let rec = rec?;
        let mut row = Row {
            line: i + 2,
            artist: get(&rec, artist),
            title: if args.albums {
                String::new()
            } else {
                get(&rec, title)
            },
            album: get(&rec, album),
            length: length.and_then(|c| parse_length(rec.get(c).unwrap_or(""), &headers[c])),
        };
        if row.is_album() && row.album.is_empty() {
            continue;
        }
        if row.is_album() {
            row.length = None;
        }
        rows.push(row);
    }
    // Album mode produces one row per track in track-level CSVs; dedupe.
    let mut seen = HashSet::new();
    rows.retain(|r| seen.insert(r.key()));
    Ok(rows)
}

// ---------- text normalization / matching ----------

pub fn norm(s: &str) -> String {
    let s = s.to_lowercase().replace('&', " and ");
    let mapped: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Distinctive artist words ("the"/"and" match everything).
fn artist_tokens(a: &str) -> Vec<String> {
    tokens(&primary_artist(a))
        .into_iter()
        .filter(|t| t.len() > 1 && !["the", "and", "of", "los", "la", "le"].contains(&t.as_str()))
        .collect()
}

fn tokens(s: &str) -> Vec<String> {
    norm(s)
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Strip decorations that rarely appear in filenames: "(Remastered 2011)", "- 2009 Remaster",
/// "[feat. X]", "(feat. X)".
fn clean_title(t: &str) -> String {
    let mut s = t.to_string();
    // Drop bracketed parts that are remaster/feat/version noise.
    for (open, close) in [('(', ')'), ('[', ']')] {
        while let Some(a) = s.find(open) {
            let Some(b) = s[a..].find(close).map(|b| a + b) else {
                break;
            };
            let inner = s[a + 1..b].to_lowercase();
            if [
                "remaster", "feat", "ft.", "with ", "mono", "stereo", "version", "edit", "deluxe",
                "bonus",
            ]
            .iter()
            .any(|k| inner.contains(k))
            {
                s.replace_range(a..=b, " ");
            } else {
                break;
            }
        }
    }
    // "Song - Remastered 2011" / "Song - 2009 Remaster" / "Song - Single Version"
    if let Some(i) = s.find(" - ") {
        let tail = s[i + 3..].to_lowercase();
        if ["remaster", "version", "mono", "stereo", "edit", "mix"]
            .iter()
            .any(|k| tail.contains(k))
            && !tail.contains("remix")
        {
            s.truncate(i);
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn primary_artist(a: &str) -> String {
    let lower = a.to_lowercase();
    let mut cut = a.len();
    for sep in [", ", ";", " feat.", " feat ", " ft.", " featuring "] {
        if let Some(i) = lower.find(sep) {
            if i > 0 {
                cut = cut.min(i);
            }
        }
    }
    a[..cut].trim().to_string()
}

/// Soulseek matches every word as a substring of the path; punctuation hurts, short noise words are fine.
fn query_for(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    norm(&joined)
}

#[derive(Debug, Clone)]
struct Candidate {
    username: String,
    files: Vec<SearchFile>,
    /// For albums: the remote folder.
    dir: String,
    label: String,
}

fn track_candidates(
    responses: &[SearchResponse],
    row: &Row,
    filter: &Filter,
    cfg: &Config,
) -> Vec<(f64, Candidate)> {
    let title_toks = tokens(&clean_title(&row.title));
    let artist_toks = artist_tokens(&row.artist);
    let (hits, _) = quality::rank(responses, filter, &cfg.prefs);
    let mut out: Vec<(f64, Candidate)> = vec![];
    let mut per_user: HashMap<String, usize> = HashMap::new();
    for h in hits {
        if !quality::is_audio(&h.file) {
            continue;
        }
        let path_n = format!(" {} ", norm(&h.file.filename));
        let base_n = format!(" {} ", norm(h.name()));
        // Every title word must be in the filename (whole-word).
        if !title_toks
            .iter()
            .all(|t| base_n.contains(&format!(" {t} ")))
        {
            continue;
        }
        let artist_hits = artist_toks
            .iter()
            .filter(|t| path_n.contains(&format!(" {t} ")))
            .count();
        if !artist_toks.is_empty() && artist_hits == 0 {
            continue;
        }
        // Reject clearly different versions unless the title asked for them.
        let want = norm(&row.title);
        if [
            "remix",
            "live",
            "instrumental",
            "karaoke",
            "acoustic",
            "cover",
        ]
        .iter()
        .any(|k| base_n.contains(&format!(" {k} ")) && !want.contains(k))
        {
            continue;
        }
        let mut score = h.score;
        match (row.length, h.file.length) {
            (Some(want), Some(got)) => {
                let diff = (want as i64 - got as i64).unsigned_abs() as u32;
                if diff > cfg.csv.length_tolerance {
                    continue;
                }
                score += 10.0 - diff as f64;
            }
            (Some(_), None) => score -= 3.0,
            _ => {}
        }
        // Extra words in the filename beyond title/artist/track numbers suggest a different version.
        let extra = tokens(h.name())
            .iter()
            .filter(|t| {
                !title_toks.contains(t)
                    && !artist_toks.contains(t)
                    && !t.chars().all(|c| c.is_ascii_digit())
            })
            .count();
        score -= (extra.saturating_sub(1)) as f64 * 1.5;
        score += artist_hits as f64;
        // At most 2 candidates per user so retries go elsewhere.
        let n = per_user.entry(h.username.clone()).or_default();
        if *n >= 2 {
            continue;
        }
        *n += 1;
        out.push((
            score,
            Candidate {
                username: h.username.clone(),
                label: quality::quality_label(&h.file),
                dir: h.dir().to_string(),
                files: vec![h.file.clone()],
            },
        ));
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out
}

fn album_candidates(
    responses: &[SearchResponse],
    row: &Row,
    filter: &Filter,
    cfg: &Config,
) -> Vec<(f64, Candidate)> {
    let album_toks = tokens(&clean_title(&row.album));
    let artist_toks = artist_tokens(&row.artist);
    let (hits, _) = quality::rank(responses, filter, &cfg.prefs);
    let hits: Vec<Hit> = hits
        .into_iter()
        .filter(|h| quality::is_audio(&h.file))
        .collect();
    let mut out = vec![];
    for f in quality::group_folders(&hits) {
        if f.audio_count() < 2 {
            continue;
        }
        let dir_n = format!(" {} ", norm(&f.dir));
        if !album_toks.iter().all(|t| dir_n.contains(&format!(" {t} "))) {
            continue;
        }
        if !artist_toks.is_empty()
            && !artist_toks
                .iter()
                .any(|t| dir_n.contains(&format!(" {t} ")))
        {
            continue;
        }
        // Consistent quality across the folder matters for albums.
        let labels: HashSet<String> = f
            .files
            .iter()
            .map(|h| quality::quality_label(&h.file))
            .collect();
        let score = f.score - (labels.len().saturating_sub(1)) as f64 * 3.0;
        let label = format!(
            "{} files, {}",
            f.files.len(),
            f.files
                .first()
                .map(|h| quality::quality_label(&h.file))
                .unwrap_or_default()
        );
        out.push((
            score,
            Candidate {
                username: f.username.clone(),
                files: f.files.iter().map(|h| h.file.clone()).collect(),
                dir: f.dir.clone(),
                label,
            },
        ));
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out
}

// ---------- state ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Pending,
    Queued,
    Done,
    Exists,
    NotFound,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RowState {
    status: Status,
    #[serde(default)]
    user: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    batch: Option<Uuid>,
    #[serde(default)]
    attempts: usize,
    #[serde(default)]
    tried: Vec<String>,
    #[serde(default)]
    queued_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl RowState {
    fn new() -> Self {
        Self {
            status: Status::Pending,
            user: String::new(),
            file: String::new(),
            batch: None,
            attempts: 0,
            tried: vec![],
            queued_at: None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JobState {
    csv: String,
    output: String,
    rows: BTreeMap<String, RowState>,
}

fn state_path(csv: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    csv.hash(&mut h);
    let stem = csv
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "csv".into());
    let dir = config::data_dir().join("csv");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{stem}-{:08x}.json", h.finish() as u32))
}

fn load_state(path: &Path) -> JobState {
    std::fs::read(path)
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, st: &JobState) {
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(st).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

// ---------- existing-file index ----------

/// Normalized paths of audio files already in the output folder.
fn existing_index(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, out: &mut Vec<String>, depth: usize) {
        if depth > 6 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else if let Some(ext) = p.extension().map(|e| e.to_string_lossy().to_lowercase()) {
                if quality::AUDIO.contains(&ext.as_str()) {
                    out.push(format!(" {} ", norm(&p.to_string_lossy())));
                }
            }
        }
    }
    let mut out = vec![];
    walk(dir, &mut out, 0);
    out
}

fn already_have(index: &[String], row: &Row) -> bool {
    let title_toks = tokens(&clean_title(if row.is_album() {
        &row.album
    } else {
        &row.title
    }));
    if title_toks.is_empty() {
        return false;
    }
    let artist_toks = artist_tokens(&row.artist);
    index.iter().any(|p| {
        title_toks.iter().all(|t| p.contains(&format!(" {t} ")))
            && (artist_toks.is_empty() || artist_toks.iter().any(|t| p.contains(&format!(" {t} "))))
    })
}

// ---------- rate limiting ----------

struct Limiter {
    max: usize,
    window: Duration,
    stamps: Mutex<VecDeque<Instant>>,
}

impl Limiter {
    async fn acquire(&self) {
        loop {
            let wait = {
                let mut s = self.stamps.lock().unwrap();
                while s
                    .front()
                    .map(|t| t.elapsed() > self.window)
                    .unwrap_or(false)
                {
                    s.pop_front();
                }
                if s.len() < self.max {
                    s.push_back(Instant::now());
                    return;
                }
                self.window.saturating_sub(s.front().unwrap().elapsed()) + Duration::from_millis(50)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

// ---------- runner ----------

struct Job {
    cfg: Config,
    client: Client,
    filter: Filter,
    fallback: Option<Filter>,
    target: Target,
    dry_run: bool,
    state: Mutex<JobState>,
    state_path: PathBuf,
    candidates: Mutex<HashMap<String, VecDeque<Candidate>>>,
    limiter: Limiter,
    total: usize,
    counter: Mutex<usize>,
    /// Consecutive searches with zero responses (connection-health heuristic).
    empty_streak: Mutex<usize>,
}

impl Job {
    fn set(&self, key: &str, f: impl FnOnce(&mut RowState)) {
        let mut st = self.state.lock().unwrap();
        let rs = st.rows.entry(key.to_string()).or_insert_with(RowState::new);
        f(rs);
        save_state(&self.state_path, &st);
    }

    fn log(&self, sym: &str, color: &str, row: &Row, detail: &str) {
        let n = {
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            *c
        };
        let w = self.total.to_string().len();
        println!(
            "[{n:>w$}/{}] \x1b[{color}m{sym}\x1b[0m {}{}",
            self.total,
            row.label(),
            if detail.is_empty() {
                String::new()
            } else {
                format!("  \x1b[2m{detail}\x1b[0m")
            }
        );
    }

    /// Whether a queued row's download is actually on disk.
    fn landed(&self, row: &Row, rs: &RowState) -> bool {
        let Target::Dir(dir) = &self.target else { return false };
        if row.is_album() {
            let name = if row.artist.is_empty() { row.album.clone() } else { format!("{} - {}", primary_artist(&row.artist), row.album) };
            let Target::Dir(d) = self.target.join(&name) else { return false };
            return std::fs::read_dir(d).map(|mut e| e.next().is_some()).unwrap_or(false);
        }
        let base = api::basename(&rs.file);
        let (stem, ext) = base.rsplit_once('.').unwrap_or((base, ""));
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten().any(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n == base || (n.starts_with(stem) && n.ends_with(ext) && n.len() <= base.len() + 5)
                })
            })
            .unwrap_or(false)
    }

    /// Result lines for transfers finishing (not numbered: the counter tracks searches).
    fn log_result(&self, sym: &str, color: &str, row: &Row, detail: &str) {
        let pad = " ".repeat(self.total.to_string().len() * 2 + 3);
        println!("{pad}\x1b[{color}m{sym}\x1b[0m {}  \x1b[2m{detail}\x1b[0m", row.label());
    }

    /// Block until slskd is logged in to Soulseek (a VPN drop disconnects it for a while).
    async fn wait_online(&self) {
        let mut warned = false;
        loop {
            match self.client.application().await {
                Ok(a) if a.server.is_logged_in => {
                    if warned {
                        println!("    \x1b[32m●\x1b[0m reconnected to Soulseek, continuing");
                    }
                    return;
                }
                _ => {
                    if !warned {
                        println!("    \x1b[33m●\x1b[0m Soulseek disconnected (VPN drop?) — waiting to reconnect…");
                        warned = true;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Search, guarding against a dead connection: one empty result is normal (obscure song),
    /// but several in a row means slskd's connection died before it noticed. In that case pause
    /// and retry the same query instead of reporting songs as not found.
    async fn search(&self, q: &str) -> Result<Vec<SearchResponse>> {
        for _ in 0..5 {
            self.wait_online().await;
            self.limiter.acquire().await;
            let responses = crate::search::run(&self.client, &self.cfg, q, |_, _| {}).await?.responses;
            if !responses.is_empty() {
                *self.empty_streak.lock().unwrap() = 0;
                return Ok(responses);
            }
            let streak = {
                let mut s = self.empty_streak.lock().unwrap();
                *s += 1;
                *s
            };
            if streak < 3 {
                return Ok(responses);
            }
            println!("    \x1b[33m●\x1b[0m {streak} searches in a row came back empty — connection problem? pausing 60s");
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Ok(vec![])
    }

    /// Candidates in preference order, plus how many matches were rejected only by quality
    /// (so "not found" can say "but a lossy copy exists").
    async fn find(&self, row: &Row) -> Result<(Vec<Candidate>, usize)> {
        let artist = primary_artist(&row.artist);
        let queries: Vec<String> = if row.is_album() {
            vec![query_for(&[&artist, &clean_title(&row.album)])]
        } else {
            let t = clean_title(&row.title);
            let mut q = vec![query_for(&[&artist, &t])];
            // Retry without the artist (matching still checks the path for it).
            if !artist.is_empty() {
                q.push(query_for(&[&t]));
            }
            q
        };
        for q in queries.iter().filter(|q| !q.is_empty()) {
            let responses = self.search(q).await?;
            let pick = |f: &Filter| {
                if row.is_album() {
                    album_candidates(&responses, row, f, &self.cfg)
                } else {
                    track_candidates(&responses, row, f, &self.cfg)
                }
            };
            let mut c = pick(&self.filter);
            if c.is_empty() {
                if let Some(fb) = &self.fallback {
                    c = pick(fb);
                }
            }
            if !c.is_empty() {
                return Ok((c.into_iter().map(|(_, c)| c).collect(), 0));
            }
            let other = pick(&Filter::default()).len();
            if other > 0 {
                return Ok((vec![], other));
            }
        }
        Ok((vec![], 0))
    }

    async fn enqueue(&self, row: &Row, c: &Candidate) -> Result<Uuid> {
        let (files, target) = if row.is_album() {
            let files =
                download::folder_contents(&self.client, &c.username, &c.dir, &c.files).await;
            let name = if row.artist.is_empty() {
                row.album.clone()
            } else {
                format!("{} - {}", primary_artist(&row.artist), row.album)
            };
            (files, self.target.join(&name))
        } else {
            (c.files.clone(), self.target.clone())
        };
        let (batch, fails) =
            download::enqueue(&self.client, &self.cfg, &c.username, &files, &target).await?;
        if fails.len() == files.len() {
            bail!("{}", fails.join("; "));
        }
        Ok(batch)
    }

    /// Queue the next untried candidate. Returns false when none are left.
    async fn try_next(&self, row: &Row) -> bool {
        let key = row.key();
        loop {
            let next = self
                .candidates
                .lock()
                .unwrap()
                .get_mut(&key)
                .and_then(|q| q.pop_front());
            let Some(c) = next else { return false };
            let id = format!(
                "{}\\{}",
                c.username,
                c.files
                    .first()
                    .map(|f| f.filename.as_str())
                    .unwrap_or(&c.dir)
            );
            let tried = self
                .state
                .lock()
                .unwrap()
                .rows
                .get(&key)
                .map(|r| r.tried.contains(&id))
                .unwrap_or(false);
            if tried {
                continue;
            }
            match self.enqueue(row, &c).await {
                Ok(batch) => {
                    self.set(&key, |r| {
                        r.status = Status::Queued;
                        r.user = c.username.clone();
                        r.file = if row.is_album() {
                            c.dir.clone()
                        } else {
                            c.files[0].filename.clone()
                        };
                        r.batch = Some(batch);
                        r.attempts += 1;
                        r.tried.push(id.clone());
                        r.queued_at = Some(chrono::Utc::now());
                    });
                    return true;
                }
                Err(_) => {
                    self.set(&key, |r| r.tried.push(id.clone()));
                }
            }
        }
    }

    async fn process(&self, row: Row) {
        let key = row.key();
        let found = match self.find(&row).await {
            Ok(c) => c,
            Err(e) => {
                self.log("!", "31", &row, &format!("search failed: {e}"));
                return;
            }
        };
        let (found, other_quality) = found;
        if found.is_empty() {
            if !self.dry_run {
                self.set(&key, |r| r.status = Status::NotFound);
            }
            let why = if other_quality > 0 {
                format!("none match the filter ({other_quality} lower-quality copies; try --fallback lossy-ok)")
            } else {
                "not found".into()
            };
            self.log("✗", "31", &row, &why);
            return;
        }
        let best = found[0].clone();
        if self.dry_run {
            self.log(
                "•",
                "36",
                &row,
                &format!(
                    "{} ← {} ({})",
                    api::basename(
                        best.files
                            .first()
                            .map(|f| f.filename.as_str())
                            .unwrap_or(&best.dir)
                    ),
                    best.username,
                    best.label
                ),
            );
            return;
        }
        let max = self.cfg.csv.max_attempts.max(1);
        self.candidates
            .lock()
            .unwrap()
            .insert(key.clone(), found.into_iter().take(max).collect());
        if self.try_next(&row).await {
            self.log(
                "↓",
                "36",
                &row,
                &format!("{} ({})", best.username, best.label),
            );
        } else {
            self.set(&key, |r| r.status = Status::Failed);
            self.log("✗", "31", &row, "couldn't queue from any source");
        }
    }
}

/// Watches queued rows until they finish; moves on to alternative sources on failure.
async fn monitor(
    job: Arc<Job>,
    rows: Arc<Vec<Row>>,
    searching_done: Arc<std::sync::atomic::AtomicBool>,
    wait: bool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let stale = Duration::from_secs(15 * 60);
    loop {
        let downloads = job.client.downloads().await.unwrap_or_default();
        download::process_moves(&job.client).await.ok();
        let queued: Vec<(Row, RowState)> = {
            let st = job.state.lock().unwrap();
            rows.iter()
                .filter_map(|r| {
                    st.rows
                        .get(&r.key())
                        .filter(|s| s.status == Status::Queued)
                        .map(|s| (r.clone(), s.clone()))
                })
                .collect()
        };
        for (row, rs) in &queued {
            let Some(batch) = rs.batch else { continue };
            let ts: Vec<&api::Transfer> = downloads
                .iter()
                .filter(|t| t.batch_id == Some(batch))
                .collect();
            if ts.is_empty() {
                // slskd no longer knows the batch (restart/retention). Trust the disk, not hope:
                // done if the file landed, otherwise failed (and retryable with --retry).
                let grace = rs.queued_at.map(|q| chrono::Utc::now() - q > chrono::Duration::minutes(2)).unwrap_or(true);
                if grace {
                    if job.landed(row, rs) {
                        job.set(&row.key(), |r| r.status = Status::Done);
                        job.log_result("✓", "32", row, &format!("from {}", rs.user));
                    } else {
                        job.set(&row.key(), |r| r.status = Status::Failed);
                        job.log_result("✗", "31", row, "slskd lost track of the transfer (retry with --retry)");
                    }
                }
                continue;
            }
            let finished = ts.iter().all(|t| t.is_finished());
            let ok = ts.iter().filter(|t| t.is_succeeded()).count();
            let started = ts.iter().any(|t| t.is_active() || t.bytes_transferred > 0);
            let too_long = !started
                && rs
                    .queued_at
                    .map(|q| (chrono::Utc::now() - q).to_std().unwrap_or_default() > stale)
                    .unwrap_or(false);
            if finished && ok * 5 >= ts.len() * 4 {
                job.set(&row.key(), |r| r.status = Status::Done);
                job.log_result("✓", "32", row, &format!("from {}", rs.user));
            } else if finished || too_long {
                for t in &ts {
                    if !t.is_finished() {
                        job.client.cancel_download(t, true).await.ok();
                    }
                }
                let why = if too_long {
                    "stuck in queue"
                } else {
                    "transfer failed"
                };
                if job.try_next(row).await {
                    let user = job
                        .state
                        .lock()
                        .unwrap()
                        .rows
                        .get(&row.key())
                        .map(|r| r.user.clone())
                        .unwrap_or_default();
                    println!("    \x1b[33m↻\x1b[0m {}: {why}, trying {user}", row.label());
                } else {
                    job.set(&row.key(), |r| r.status = Status::Failed);
                    job.log_result("✗", "31", row, why);
                }
            }
        }
        let still_queued = queued.len();
        if searching_done.load(Ordering::SeqCst) && (!wait || still_queued == 0) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn print_status(rows: &[Row], st: &JobState) {
    let mut counts: BTreeMap<Status, usize> = BTreeMap::new();
    for r in rows {
        *counts
            .entry(
                st.rows
                    .get(&r.key())
                    .map(|s| s.status)
                    .unwrap_or(Status::Pending),
            )
            .or_default() += 1;
    }
    let get = |s| counts.get(&s).copied().unwrap_or(0);
    println!(
        "{} rows: \x1b[32m{} done\x1b[0m, {} already had, \x1b[36m{} downloading\x1b[0m, \x1b[31m{} not found, {} failed\x1b[0m, {} pending",
        rows.len(),
        get(Status::Done),
        get(Status::Exists),
        get(Status::Queued),
        get(Status::NotFound),
        get(Status::Failed),
        get(Status::Pending)
    );
}

pub async fn run(cfg: &Config, args: CsvArgs) -> Result<()> {
    let csv_path = config::expand(&args.file)
        .canonicalize()
        .with_context(|| format!("{} not found", args.file))?;
    let rows = read_rows(&csv_path, &args)?;
    let spath = state_path(&csv_path);
    if args.restart {
        let _ = std::fs::remove_file(&spath);
    }
    let mut st = load_state(&spath);

    let stem = csv_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "csv".into());
    let out_dir = match (&args.output, st.output.is_empty()) {
        (Some(o), _) => config::expand(o),
        (None, false) => PathBuf::from(&st.output),
        (None, true) => cfg.downloads_dir().join(&stem),
    };
    st.csv = csv_path.display().to_string();
    st.output = out_dir.display().to_string();

    if args.status {
        println!("{} → {}", config::tilde(&csv_path), config::tilde(&out_dir));
        print_status(&rows, &st);
        let bad: Vec<_> = rows
            .iter()
            .filter(|r| {
                matches!(
                    st.rows.get(&r.key()).map(|s| s.status),
                    Some(Status::NotFound | Status::Failed)
                )
            })
            .collect();
        if !bad.is_empty() {
            println!("\nnot downloaded (retry with --retry, or try --fallback lossy-ok):");
            for r in bad.iter().take(50) {
                println!("  line {:>5}  {}", r.line, r.label());
            }
            if bad.len() > 50 {
                println!("  … and {} more", bad.len() - 50);
            }
        }
        return Ok(());
    }

    let (label, filter) = crate::cli::build_filter(cfg, &args.filter, &cfg.csv.preset)?;
    let fallback_name = args
        .fallback
        .clone()
        .or_else(|| Some(cfg.csv.fallback_preset.clone()).filter(|s| !s.is_empty()));
    let fallback = fallback_name
        .as_deref()
        .map(|n| cfg.preset(n))
        .transpose()?;

    // Decide what to process.
    let index = existing_index(&out_dir);
    let mut todo = vec![];
    for r in rows.iter().skip(args.offset) {
        let status = st
            .rows
            .get(&r.key())
            .map(|s| s.status)
            .unwrap_or(Status::Pending);
        let want = match status {
            Status::Pending => true,
            Status::NotFound | Status::Failed => args.retry,
            Status::Queued => false, // resumed by the monitor below
            Status::Done | Status::Exists => false,
        };
        if !want {
            continue;
        }
        if already_have(&index, r) {
            st.rows.entry(r.key()).or_insert_with(RowState::new).status = Status::Exists;
            continue;
        }
        if args.retry {
            if let Some(s) = st.rows.get_mut(&r.key()) {
                s.tried.clear();
            }
        }
        todo.push(r.clone());
        if args.number.map(|n| todo.len() >= n).unwrap_or(false) {
            break;
        }
    }
    save_state(&spath, &st);

    let resumed = rows
        .iter()
        .filter(|r| {
            st.rows
                .get(&r.key())
                .map(|s| s.status == Status::Queued)
                .unwrap_or(false)
        })
        .count();
    println!(
        "\x1b[1m{}\x1b[0m → {}",
        config::tilde(&csv_path),
        config::tilde(&out_dir)
    );
    print_status(&rows, &st);
    println!(
        "filter: {label} ({}){} · {} to search{}",
        filter.describe(),
        fallback_name
            .as_ref()
            .map(|f| format!(", fallback {f}"))
            .unwrap_or_default(),
        todo.len(),
        if resumed > 0 {
            format!(", {resumed} in progress from last run")
        } else {
            String::new()
        }
    );
    if todo.is_empty() && resumed == 0 {
        println!(
            "nothing to do{}",
            if !args.retry {
                " (use --retry to retry not-found/failed rows)"
            } else {
                ""
            }
        );
        return Ok(());
    }
    if todo.len() > cfg.csv.searches_per_window {
        let mins = todo.len() as f64 / cfg.csv.searches_per_window as f64
            * cfg.csv.window_secs as f64
            / 60.0;
        println!(
            "\x1b[2mSoulseek limits searches to ~{} per {}s, so this takes at least ~{:.0} min. Ctrl-C is safe; rerun to resume.\x1b[0m",
            cfg.csv.searches_per_window, cfg.csv.window_secs, mins
        );
    }
    println!();

    let client = Client::new(cfg)?;
    let job = Arc::new(Job {
        cfg: cfg.clone(),
        client,
        filter,
        fallback,
        target: Target::Dir(out_dir.clone()),
        dry_run: args.dry_run,
        state: Mutex::new(st),
        state_path: spath.clone(),
        candidates: Mutex::new(HashMap::new()),
        limiter: Limiter {
            max: cfg.csv.searches_per_window.max(1),
            window: Duration::from_secs(cfg.csv.window_secs),
            stamps: Mutex::new(VecDeque::new()),
        },
        total: todo.len(),
        counter: Mutex::new(0),
        empty_streak: Mutex::new(0),
    });

    let done_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let all_rows = Arc::new(rows);
    let mon = if args.dry_run {
        None
    } else {
        Some(tokio::spawn(monitor(
            job.clone(),
            all_rows.clone(),
            done_flag.clone(),
            !args.no_wait,
        )))
    };

    let work = futures::stream::iter(todo.into_iter().map(|r| {
        let job = job.clone();
        async move { job.process(r).await }
    }))
    .buffer_unordered(cfg.csv.concurrency.max(1))
    .collect::<Vec<_>>();

    tokio::select! {
        _ = work => {}
        _ = tokio::signal::ctrl_c() => {
            println!("\ninterrupted — progress saved; rerun the same command to resume");
            return Ok(());
        }
    }
    done_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(m) = mon {
        if !args.no_wait {
            println!(
                "\x1b[2mall searches done; waiting for downloads to finish (Ctrl-C to stop waiting — they keep going in slskd)\x1b[0m"
            );
        }
        tokio::select! {
            r = m => { r??; }
            _ = tokio::signal::ctrl_c() => {
                println!("\nstopped waiting — downloads continue in slskd; rerun to check on them");
                return Ok(());
            }
        }
    }
    println!();
    let st = job.state.lock().unwrap();
    print_status(&all_rows, &st);
    if download::pending_count() > 0 && !crate::daemon::is_active(crate::daemon::AGENT_UNIT) {
        println!(
            "\x1b[33mnote:\x1b[0m some files are still staged; the agent (`vibeseek agent install`) moves them when done"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_titles() {
        assert_eq!(
            clean_title("Time Is Running Out - Remastered 2011"),
            "Time Is Running Out"
        );
        assert_eq!(clean_title("Song (feat. Someone)"), "Song");
        assert_eq!(
            clean_title("Song (Live at Wembley)"),
            "Song (Live at Wembley)"
        );
        assert_eq!(
            clean_title("Around the World - Radio Edit"),
            "Around the World"
        );
        assert_eq!(
            clean_title("Song - Daft Punk Remix"),
            "Song - Daft Punk Remix"
        );
    }

    #[test]
    fn primary_artists() {
        assert_eq!(primary_artist("Daft Punk, Pharrell Williams"), "Daft Punk");
        assert_eq!(primary_artist("Simon & Garfunkel"), "Simon & Garfunkel");
        assert_eq!(primary_artist("Kanye West feat. Jay-Z"), "Kanye West");
        assert_eq!(
            primary_artist("The Dave Brubeck Quartet"),
            "The Dave Brubeck Quartet"
        );
    }

    #[test]
    fn lengths() {
        assert_eq!(parse_length("3:47", "duration"), Some(227));
        assert_eq!(parse_length("227333", "duration_ms"), Some(227));
        assert_eq!(parse_length("227", "length"), Some(227));
        assert_eq!(parse_length("1:02:03", "length"), Some(3723));
    }
}
