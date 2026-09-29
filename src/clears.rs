//! Manual "clear finished" for the transfer lists. Clearing only tidies the view: vibeseek
//! remembers when you last cleared and hides finished transfers from before then. Uploads stay
//! in slskd and in your upload history; `--all` still shows everything.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::Transfer;
use crate::config::data_dir;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Clears {
    pub uploads: Option<DateTime<Utc>>,
    pub downloads: Option<DateTime<Utc>>,
}

impl Clears {
    pub fn mark(&self, uploads: bool) -> Option<DateTime<Utc>> {
        if uploads { self.uploads } else { self.downloads }
    }
}

fn path() -> std::path::PathBuf {
    data_dir().join("cleared.json")
}

pub fn load() -> Clears {
    std::fs::read(path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

/// Clear everything finished up to now from the uploads or downloads list.
pub fn clear_now(uploads: bool) -> Result<Clears> {
    let mut c = load();
    let now = Some(Utc::now());
    if uploads {
        c.uploads = now;
    } else {
        c.downloads = now;
    }
    std::fs::write(path(), serde_json::to_vec_pretty(&c)?)?;
    Ok(c)
}

/// Finished before the last clear? (Active and queued transfers are never hidden.)
pub fn is_cleared(t: &Transfer, mark: Option<DateTime<Utc>>) -> bool {
    match mark {
        Some(m) if t.is_finished() => t.ended_at.or(t.requested_at).map(|e| e <= m).unwrap_or(true),
        _ => false,
    }
}
