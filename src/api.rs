//! Thin async client for the slskd REST API (v0).

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

use crate::config::Config;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
}

// ---------- models ----------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerState {
    pub state: String,
    pub is_connected: bool,
    pub is_logged_in: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AppState {
    pub server: ServerState,
    pub user: AppUser,
    pub shares: SharesState,
    pub pending_restart: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AppUser {
    pub username: String,
    pub statistics: UserStats,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct UserStats {
    pub average_speed: f64,
    pub directory_count: u64,
    pub file_count: u64,
    pub upload_count: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SharesState {
    pub scanning: bool,
    pub ready: bool,
    pub scan_progress: f64,
    pub files: u64,
    pub directories: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Search {
    pub id: Uuid,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub is_complete: bool,
    #[serde(default)]
    pub response_count: u32,
    #[serde(default)]
    pub file_count: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub username: String,
    #[serde(default)]
    pub files: Vec<SearchFile>,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub queue_length: u64,
    #[serde(default)]
    pub upload_speed: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchFile {
    pub filename: String,
    pub size: u64,
    pub extension: String,
    pub bit_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
    pub length: Option<u32>,
    pub is_variable_bit_rate: Option<bool>,
    pub is_locked: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTransfers {
    #[serde(default)]
    pub directories: Vec<DirTransfers>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirTransfers {
    #[serde(default)]
    pub files: Vec<Transfer>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: Uuid,
    pub username: String,
    #[serde(default)]
    pub direction: String,
    pub filename: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub state: String,
    #[serde(default, deserialize_with = "lenient_date")]
    pub requested_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "lenient_date")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "lenient_date")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub average_speed: f64,
    #[serde(default)]
    pub place_in_queue: Option<u32>,
    #[serde(default)]
    pub exception: Option<String>,
    #[serde(default)]
    pub percent_complete: f64,
    #[serde(default)]
    pub batch_id: Option<Uuid>,
}

impl Transfer {
    /// slskd states are flag strings like "Completed, Succeeded" or "Queued, Remotely".
    pub fn is_finished(&self) -> bool {
        self.state.starts_with("Completed")
    }
    pub fn is_succeeded(&self) -> bool {
        self.state.contains("Succeeded")
    }
    pub fn is_failed(&self) -> bool {
        self.is_finished() && !self.is_succeeded()
    }
    pub fn is_active(&self) -> bool {
        self.state.starts_with("InProgress") || self.state.starts_with("Initializing")
    }
    pub fn is_queued(&self) -> bool {
        self.state.starts_with("Queued") || self.state.starts_with("Requested")
    }
    /// Short human label for the state.
    pub fn short_state(&self) -> String {
        let s = &self.state;
        if s.starts_with("Completed") {
            s.trim_start_matches("Completed, ").to_string()
        } else if s.starts_with("Queued") {
            match self.place_in_queue {
                Some(p) if p > 0 => format!("Queued #{p}"),
                _ => s.replace("Queued, ", "Queued ").replace("Remotely", "(remote)").replace("Locally", "(local)"),
            }
        } else if s.starts_with("InProgress") {
            "Transferring".into()
        } else {
            s.clone()
        }
    }
    pub fn basename(&self) -> &str {
        basename(&self.filename)
    }
}

/// slskd emits some timestamps without a timezone ("2026-09-26T21:39:24.3422648"); they're UTC.
fn lenient_date<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    let Some(s) = Option::<String>::deserialize(d)? else { return Ok(None) };
    if let Ok(t) = DateTime::parse_from_rfc3339(&s) {
        return Ok(Some(t.with_timezone(&Utc)));
    }
    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f")
        .map(|n| Some(n.and_utc()))
        .map_err(serde::de::Error::custom)
}

/// Soulseek paths use backslashes.
pub fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

pub fn dirname(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(i) => &path[..i],
        None => "",
    }
}

pub fn flatten(users: Vec<UserTransfers>) -> Vec<Transfer> {
    let mut out: Vec<Transfer> = users
        .into_iter()
        .flat_map(|u| u.directories.into_iter().flat_map(|d| d.files.into_iter()))
        .collect();
    out.sort_by_key(|t| std::cmp::Reverse(t.requested_at));
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueFile {
    pub filename: String,
    pub size: u64,
}

// ---------- client ----------

impl Client {
    pub fn new(cfg: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()?;
        Ok(Self {
            http,
            base: format!("{}/api/v0", cfg.slskd.url.trim_end_matches('/')),
            key: cfg.api_key()?,
        })
    }

    async fn req<T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<T> {
        let text = self.raw(method, path, body).await?;
        let text = if text.trim().is_empty() { "null".into() } else { text };
        serde_json::from_str(&text).with_context(|| format!("decoding response from {path}"))
    }

    async fn raw(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<String> {
        let url = format!("{}{}", self.base, path);
        let mut rb = self.http.request(method.clone(), &url).header("X-API-Key", &self.key);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().await.map_err(|e| {
            if e.is_connect() {
                anyhow::anyhow!("can't reach slskd at {} (is it running? try `vibeseek daemon start`)", self.base)
            } else {
                anyhow::Error::new(e)
            }
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                bail!("slskd rejected the API key ({status})");
            }
            bail!("{method} {path} failed: {status} {}", text.chars().take(300).collect::<String>());
        }
        Ok(text)
    }

    pub async fn application(&self) -> Result<AppState> {
        self.req(Method::GET, "/application", None).await
    }

    pub async fn start_search(&self, text: &str, timeout_ms: u64, response_limit: u32, file_limit: u32) -> Result<Search> {
        let body = json!({
            "id": Uuid::new_v4(),
            "searchText": text,
            "searchTimeout": timeout_ms,
            "responseLimit": response_limit,
            "fileLimit": file_limit,
            "filterResponses": true,
            "minimumResponseFileCount": 1,
        });
        // slskd only starts one search at a time and answers 429 to the rest; wait our turn.
        let mut delay = 300;
        for _ in 0..40 {
            match self.req(Method::POST, "/searches", Some(body.clone())).await {
                Err(e) if e.to_string().contains("429") => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay = (delay * 3 / 2).min(3000);
                }
                other => return other,
            }
        }
        bail!("slskd kept refusing to start the search (too many concurrent searches)")
    }

    pub async fn get_search(&self, id: Uuid) -> Result<Search> {
        self.req(Method::GET, &format!("/searches/{id}"), None).await
    }

    pub async fn search_responses(&self, id: Uuid) -> Result<Vec<SearchResponse>> {
        self.req(Method::GET, &format!("/searches/{id}/responses"), None).await
    }

    pub async fn stop_search(&self, id: Uuid) -> Result<()> {
        self.raw(Method::PUT, &format!("/searches/{id}"), None).await.map(|_| ())
    }


    /// Enqueue downloads from one user. `destination` is relative to slskd's downloads dir.
    /// Returns the batch id and any per-file failure messages.
    pub async fn enqueue(&self, username: &str, files: &[QueueFile], destination: Option<&str>) -> Result<(Uuid, Vec<String>)> {
        let id = Uuid::new_v4();
        let mut opts = serde_json::Map::new();
        if let Some(d) = destination {
            opts.insert("destination".into(), json!(d));
        }
        let body = json!({
            "id": id,
            "username": username,
            "files": files,
            "options": opts,
        });
        let resp: serde_json::Value = self.req(Method::POST, "/transfers/downloads/batches", Some(body)).await?;
        let failures = resp
            .get("failures")
            .and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .map(|f| {
                        format!(
                            "{}: {}",
                            basename(f.get("filename").and_then(|v| v.as_str()).unwrap_or("?")),
                            f.get("message").and_then(|v| v.as_str()).unwrap_or("failed")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((id, failures))
    }

    pub async fn downloads(&self) -> Result<Vec<Transfer>> {
        let users: Vec<UserTransfers> = self.req(Method::GET, "/transfers/downloads", None).await?;
        Ok(flatten(users))
    }

    pub async fn uploads(&self) -> Result<Vec<Transfer>> {
        let users: Vec<UserTransfers> = self.req(Method::GET, "/transfers/uploads", None).await?;
        Ok(flatten(users))
    }

    pub async fn cancel_download(&self, t: &Transfer, remove: bool) -> Result<()> {
        let path = format!("/transfers/downloads/{}/{}?remove={remove}", enc(&t.username), t.id);
        self.raw(Method::DELETE, &path, None).await.map(|_| ())
    }

    pub async fn cancel_upload(&self, t: &Transfer, remove: bool) -> Result<()> {
        let path = format!("/transfers/uploads/{}/{}?remove={remove}", enc(&t.username), t.id);
        self.raw(Method::DELETE, &path, None).await.map(|_| ())
    }

    pub async fn clear_completed_downloads(&self) -> Result<()> {
        self.raw(Method::DELETE, "/transfers/downloads/all/completed", None).await.map(|_| ())
    }

    pub async fn clear_completed_uploads(&self) -> Result<()> {
        self.raw(Method::DELETE, "/transfers/uploads/all/completed", None).await.map(|_| ())
    }

    /// List the files in one remote folder. Returned filenames are full remote paths.
    pub async fn browse_dir(&self, username: &str, dir: &str) -> Result<Vec<SearchFile>> {
        let v: serde_json::Value = self
            .req(Method::POST, &format!("/users/{}/directory", enc(username)), Some(json!({ "directory": dir })))
            .await?;
        // Soulseek.NET returns a list of directories (or a single one) with bare file names.
        let dirs = match v {
            serde_json::Value::Array(a) => a,
            other => vec![other],
        };
        let mut out = vec![];
        for d in dirs {
            let name = d.get("name").and_then(|n| n.as_str()).unwrap_or(dir).to_string();
            if let Some(files) = d.get("files").and_then(|f| f.as_array()) {
                for f in files {
                    let mut sf: SearchFile = serde_json::from_value(f.clone()).unwrap_or_default();
                    if !sf.filename.contains('\\') {
                        sf.filename = format!("{name}\\{}", sf.filename);
                    }
                    out.push(sf);
                }
            }
        }
        Ok(out)
    }

}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
