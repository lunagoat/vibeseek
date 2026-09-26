//! Queueing downloads into arbitrary folders.
//!
//! slskd can only place a batch *relative to its downloads directory*. For targets inside it we
//! use that directly. For anything else we download into a staging folder and move completed
//! files to the target (done by `vibeseek agent`, the TUI, and `get --wait`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::api::{Client, QueueFile, SearchFile};
use crate::config::{data_dir, Config};

const STAGING: &str = ".vibeseek-staging";

#[derive(Debug, Clone)]
pub enum Target {
    /// Let slskd organize it (downloads dir / source folder name).
    Default,
    /// Put files directly in this folder.
    Dir(PathBuf),
}

impl Target {
    pub fn from_opt(cfg: &Config, out: Option<&str>) -> Target {
        match out.filter(|s| !s.is_empty()) {
            Some(p) => Target::Dir(crate::config::expand(p)),
            None if !cfg.search.default_output.is_empty() => Target::Dir(crate::config::expand(&cfg.search.default_output)),
            None => Target::Default,
        }
    }

    pub fn join(&self, sub: &str) -> Target {
        match self {
            Target::Default => Target::Default,
            Target::Dir(p) => Target::Dir(p.join(sanitize(sub))),
        }
    }

    pub fn describe(&self, cfg: &Config) -> String {
        match self {
            Target::Default => format!("{} (by source folder)", crate::config::tilde(&cfg.downloads_dir())),
            Target::Dir(p) => crate::config::tilde(p),
        }
    }
}

fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | '\0') { '_' } else { c })
        .collect();
    let t = cleaned.trim().trim_matches('.').trim();
    if t.is_empty() { "folder".into() } else { t.to_string() }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMove {
    pub batch: Uuid,
    pub staging: PathBuf,
    pub target: PathBuf,
    pub created: chrono::DateTime<chrono::Utc>,
}

/// Queue `files` from `username` into `target`. Returns (batch id, failures).
pub async fn enqueue(client: &Client, cfg: &Config, username: &str, files: &[SearchFile], target: &Target) -> Result<(Uuid, Vec<String>)> {
    let q: Vec<QueueFile> = files.iter().map(|f| QueueFile { filename: f.filename.clone(), size: f.size }).collect();
    match target {
        Target::Default => client.enqueue(username, &q, None).await,
        Target::Dir(dir) => {
            let dl = cfg.downloads_dir();
            if let Ok(rel) = dir.strip_prefix(&dl) {
                if !rel.as_os_str().is_empty() && !rel.starts_with(STAGING) {
                    return client.enqueue(username, &q, Some(&rel.to_string_lossy())).await;
                }
            }
            let stage_rel = format!("{STAGING}/{}", Uuid::new_v4());
            let (batch, failures) = client.enqueue(username, &q, Some(&stage_rel)).await?;
            if failures.len() < files.len() {
                register(PendingMove { batch, staging: dl.join(&stage_rel), target: dir.clone(), created: chrono::Utc::now() })?;
            }
            Ok((batch, failures))
        }
    }
}

/// Fetch the complete contents of a remote folder (so album downloads include every track and
/// the cover). Falls back to `known` when the peer doesn't answer.
pub async fn folder_contents(client: &Client, username: &str, dir: &str, known: &[SearchFile]) -> Vec<SearchFile> {
    match tokio::time::timeout(std::time::Duration::from_secs(20), client.browse_dir(username, dir)).await {
        Ok(Ok(files)) if !files.is_empty() => files,
        _ => known.to_vec(),
    }
}

fn moves_path() -> PathBuf {
    data_dir().join("pending_moves.json")
}

/// Run `f` on the pending-move list while holding an exclusive file lock.
fn with_moves<T>(f: impl FnOnce(&mut Vec<PendingMove>) -> T) -> Result<T> {
    let mut file = File::options().read(true).write(true).create(true).truncate(false).open(moves_path())?;
    file.lock()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut list: Vec<PendingMove> = if buf.trim().is_empty() { vec![] } else { serde_json::from_str(&buf).unwrap_or_default() };
    let before = serde_json::to_string(&list)?;
    let out = f(&mut list);
    let after = serde_json::to_string_pretty(&list)?;
    if before != serde_json::to_string(&list)? {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(after.as_bytes())?;
    }
    Ok(out)
}

fn register(m: PendingMove) -> Result<()> {
    with_moves(|l| l.push(m))
}

pub fn pending_count() -> usize {
    with_moves(|l| l.len()).unwrap_or(0)
}

/// Move finished files out of staging. Returns the number of files moved.
pub async fn process_moves(client: &Client) -> Result<usize> {
    if with_moves(|l| l.is_empty())? {
        return Ok(0);
    }
    let downloads = client.downloads().await?;
    with_moves(|list| {
        let mut moved = 0;
        list.retain(|m| {
            moved += move_dir_contents(&m.staging, &m.target).unwrap_or(0);
            let unfinished = downloads.iter().any(|t| t.batch_id == Some(m.batch) && !t.is_finished());
            let empty = std::fs::read_dir(&m.staging).map(|mut d| d.next().is_none()).unwrap_or(true);
            // Only forget an entry once slskd is done with the batch and staging is empty,
            // however long that takes; otherwise later files would be stranded in staging.
            if !unfinished && empty {
                let _ = std::fs::remove_dir(&m.staging);
                false
            } else {
                true
            }
        });
        moved
    })
}

fn move_dir_contents(from: &Path, to: &Path) -> Result<usize> {
    let Ok(entries) = std::fs::read_dir(from) else { return Ok(0) };
    let mut n = 0;
    for e in entries.flatten() {
        let src = e.path();
        if !src.is_file() {
            continue;
        }
        std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        let dest = unique_path(&to.join(e.file_name()));
        if std::fs::rename(&src, &dest).is_err() {
            // Cross-device: copy then delete.
            std::fs::copy(&src, &dest)?;
            std::fs::remove_file(&src)?;
        }
        n += 1;
    }
    Ok(n)
}

fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 1.. {
        let cand = p.with_file_name(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

