//! Running searches against slskd and caching the last result set for `vibeseek get`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::api::{Client, SearchFile, SearchResponse};
use crate::config::{cache_dir, Config};
use crate::quality::Hit;

pub struct SearchOutcome {
    pub responses: Vec<SearchResponse>,
    pub state: String,
}

/// Start a search and poll until slskd marks it complete (or the timeout passes).
/// `progress(responses, files)` is called on each poll.
pub async fn run(client: &Client, cfg: &Config, text: &str, mut progress: impl FnMut(u32, u32)) -> Result<SearchOutcome> {
    let timeout_ms = cfg.search.timeout_secs * 1000;
    let s = client
        .start_search(text, timeout_ms, cfg.search.response_limit, cfg.search.file_limit)
        .await?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms + 5000);
    let mut state;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cur = client.get_search(s.id).await?;
        progress(cur.response_count, cur.file_count);
        state = cur.state.clone();
        if cur.is_complete || Instant::now() > deadline {
            break;
        }
    }
    if !state.starts_with("Completed") {
        let _ = client.stop_search(s.id).await;
    }
    let responses = client.search_responses(s.id).await?;
    // Keep slskd's search list tidy; results live in our cache.
    let _ = client.delete_search(s.id).await;
    Ok(SearchOutcome { responses, state })
}

/// Last search, persisted so `vibeseek get 3 5-7` can refer to numbered results.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct LastSearch {
    pub query: String,
    /// true when the numbers refer to folders rather than files.
    pub folders: bool,
    pub items: Vec<CachedItem>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CachedItem {
    pub username: String,
    /// For folder items, the folder path; for file items, the file path.
    pub path: String,
    pub files: Vec<SearchFile>,
}

impl CachedItem {
    pub fn from_hit(h: &Hit) -> Self {
        Self { username: h.username.clone(), path: h.file.filename.clone(), files: vec![h.file.clone()] }
    }
}

fn last_path() -> std::path::PathBuf {
    cache_dir().join("last_search.json")
}

pub fn save_last(last: &LastSearch) -> Result<()> {
    std::fs::write(last_path(), serde_json::to_vec(last)?)?;
    Ok(())
}

pub fn load_last() -> Result<LastSearch> {
    let data = std::fs::read(last_path())
        .map_err(|_| anyhow::anyhow!("no previous search; run `vibeseek search <query>` first"))?;
    Ok(serde_json::from_slice(&data)?)
}

/// Parse "1 3 5-7,9" into 1-based indices.
pub fn parse_selection(args: &[String]) -> Result<Vec<usize>> {
    let mut out = vec![];
    for a in args {
        for part in a.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((lo, hi)) = part.split_once('-') {
                let (lo, hi): (usize, usize) = (lo.parse()?, hi.parse()?);
                if lo == 0 || hi < lo {
                    anyhow::bail!("bad range '{part}'");
                }
                out.extend(lo..=hi);
            } else {
                let n: usize = part.parse().map_err(|_| anyhow::anyhow!("bad selection '{part}'"))?;
                if n == 0 {
                    anyhow::bail!("results are numbered from 1");
                }
                out.push(n);
            }
        }
    }
    out.dedup();
    Ok(out)
}
