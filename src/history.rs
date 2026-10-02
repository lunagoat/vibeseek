//! Permanent upload history (slskd forgets finished transfers after its retention window).
//! Recorded by `vibeseek agent`, the TUI, and `vibeseek uploads`.
//! Also remembers where each download batch was sent, for "open in Dolphin".

use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::api::Transfer;
use crate::config::data_dir;

pub struct History {
    db: Connection,
}

#[derive(Debug, Clone, Default)]
pub struct Totals {
    pub files: u64,
    pub bytes: u64,
    pub users: u64,
    pub failed: u64,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub ended_at: String,
    pub username: String,
    pub filename: String,
    pub size: u64,
    pub bytes: u64,
    pub state: String,
}

impl History {
    pub fn open() -> Result<Self> {
        Self::with(Connection::open(data_dir().join("history.db"))?)
    }

    fn with(db: Connection) -> Result<Self> {
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS uploads (
                id TEXT PRIMARY KEY,
                username TEXT NOT NULL,
                filename TEXT NOT NULL,
                size INTEGER NOT NULL,
                bytes INTEGER NOT NULL,
                state TEXT NOT NULL,
                speed REAL NOT NULL,
                started_at TEXT,
                ended_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS uploads_user ON uploads(username);
            CREATE INDEX IF NOT EXISTS uploads_ended ON uploads(ended_at);
            CREATE TABLE IF NOT EXISTS destinations (
                batch TEXT PRIMARY KEY,
                dir TEXT NOT NULL,
                queued_at TEXT NOT NULL
            );",
        )?;
        Ok(Self { db })
    }

    /// Remember which folder a download batch was queued into (slskd doesn't report it back).
    pub fn remember_destination(&self, batch: Uuid, dir: &Path) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO destinations (batch, dir, queued_at) VALUES (?1, ?2, ?3)",
            params![batch.to_string(), dir.to_string_lossy(), chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn destination(&self, batch: Uuid) -> Result<Option<PathBuf>> {
        let mut stmt = self.db.prepare_cached("SELECT dir FROM destinations WHERE batch = ?1")?;
        let mut rows = stmt.query_map([batch.to_string()], |r| r.get::<_, String>(0))?;
        Ok(rows.next().transpose()?.map(PathBuf::from))
    }

    /// Record finished uploads. Returns how many were new.
    pub fn record(&self, uploads: &[Transfer]) -> Result<usize> {
        let mut stmt = self.db.prepare_cached(
            "INSERT OR IGNORE INTO uploads (id, username, filename, size, bytes, state, speed, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut n = 0;
        for t in uploads.iter().filter(|t| t.is_finished()) {
            let ended = t.ended_at.unwrap_or_else(chrono::Utc::now).to_rfc3339();
            n += stmt.execute(params![
                t.id.to_string(),
                t.username,
                t.filename,
                t.size as i64,
                t.bytes_transferred as i64,
                t.state,
                t.average_speed,
                t.started_at.map(|d| d.to_rfc3339()),
                ended,
            ])?;
        }
        Ok(n)
    }

    /// `since`: RFC3339 lower bound, or None for all time.
    pub fn totals(&self, since: Option<&str>) -> Result<Totals> {
        let since = since.unwrap_or("");
        Ok(self.db.query_row(
            "SELECT
                COALESCE(SUM(state LIKE '%Succeeded'), 0),
                COALESCE(SUM(CASE WHEN state LIKE '%Succeeded' THEN bytes ELSE 0 END), 0),
                COUNT(DISTINCT CASE WHEN state LIKE '%Succeeded' THEN username END),
                COALESCE(SUM(state NOT LIKE '%Succeeded'), 0)
             FROM uploads WHERE ended_at >= ?1",
            [since],
            |r| Ok(Totals { files: r.get::<_, i64>(0)? as u64, bytes: r.get::<_, i64>(1)? as u64, users: r.get::<_, i64>(2)? as u64, failed: r.get::<_, i64>(3)? as u64 }),
        )?)
    }

    /// (username, files, bytes) sorted by bytes.
    pub fn top_users(&self, limit: usize, since: Option<&str>) -> Result<Vec<(String, u64, u64)>> {
        let mut stmt = self.db.prepare(
            "SELECT username, COUNT(*), SUM(bytes) FROM uploads
             WHERE state LIKE '%Succeeded' AND ended_at >= ?1
             GROUP BY username ORDER BY SUM(bytes) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since.unwrap_or(""), limit as i64], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// (filename, times, distinct users) sorted by times downloaded.
    pub fn top_files(&self, limit: usize, since: Option<&str>) -> Result<Vec<(String, u64, u64)>> {
        let mut stmt = self.db.prepare(
            "SELECT filename, COUNT(*), COUNT(DISTINCT username) FROM uploads
             WHERE state LIKE '%Succeeded' AND ended_at >= ?1
             GROUP BY filename ORDER BY COUNT(*) DESC, MAX(ended_at) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since.unwrap_or(""), limit as i64], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn recent(&self, limit: usize, user: Option<&str>) -> Result<Vec<Row>> {
        let mut stmt = self.db.prepare(
            "SELECT ended_at, username, filename, size, bytes, state FROM uploads
             WHERE (?1 IS NULL OR username = ?1)
             ORDER BY ended_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user, limit as i64], |r| {
            Ok(Row {
                ended_at: r.get(0)?,
                username: r.get(1)?,
                filename: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                bytes: r.get::<_, i64>(4)? as u64,
                state: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_round_trip() {
        let h = History::with(Connection::open_in_memory().unwrap()).unwrap();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        h.remember_destination(a, Path::new("/music/My Album")).unwrap();
        assert_eq!(h.destination(a).unwrap(), Some(PathBuf::from("/music/My Album")));
        assert_eq!(h.destination(b).unwrap(), None);
    }
}
