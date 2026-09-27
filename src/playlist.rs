//! Turning Spotify and YouTube links into track lists for the batch downloader.
//!
//! - Spotify playlists, albums, tracks and liked songs, via the Web API with your developer
//!   app (`[spotify]` in config.toml). Playlists and liked songs need `vibeseek spotify login`
//!   once; albums and tracks work with just the app credentials.
//! - YouTube / YouTube Music playlists and videos, via `yt-dlp --flat-playlist`.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;

use crate::config::Config;
use crate::csvjob::Row;

pub struct Playlist {
    pub name: String,
    pub rows: Vec<Row>,
}

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("spotify:") || s == "spotify-likes"
}

pub async fn fetch(cfg: &Config, url: &str) -> Result<Playlist> {
    if let Some((kind, id)) = spotify_ref(url) {
        return spotify(cfg, kind, &id).await;
    }
    if url.contains("youtube.com") || url.contains("youtu.be") {
        let url = url.to_string();
        return tokio::task::spawn_blocking(move || youtube(&url)).await?;
    }
    bail!("unsupported link (Spotify playlist/album/track or YouTube playlist/video): {url}")
}

// ---------- Spotify ----------

/// ("playlist" | "album" | "track", id) from open.spotify.com links or spotify: URIs.
fn spotify_ref(url: &str) -> Option<(&'static str, String)> {
    if url == "spotify-likes" || url == "spotify:liked" || url.contains("open.spotify.com/collection/tracks") {
        return Some(("liked", String::new()));
    }
    let rest = if let Some(r) = url.strip_prefix("spotify:") {
        r.replace(':', "/")
    } else {
        let i = url.find("open.spotify.com/")?;
        url[i + "open.spotify.com/".len()..].to_string()
    };
    let mut parts = rest.split('/').filter(|p| !p.is_empty() && !p.starts_with("intl-"));
    let kind = match parts.next()? {
        "playlist" => "playlist",
        "album" => "album",
        "track" => "track",
        _ => return None,
    };
    let id: String = parts.next()?.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!id.is_empty()).then_some((kind, id))
}

const SCOPES: &str = "playlist-read-private playlist-read-collaborative user-library-read";

fn token_path() -> std::path::PathBuf {
    crate::config::data_dir().join("spotify_token.json")
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn check_app(cfg: &Config) -> Result<()> {
    if cfg.spotify.client_id.is_empty() || cfg.spotify.client_secret.is_empty() {
        bail!(
            "Spotify links need a developer app: set client_id and client_secret under [spotify] in {}\n\
             (create one at https://developer.spotify.com/dashboard)",
            crate::config::config_file().display()
        );
    }
    Ok(())
}

/// POST to Spotify's token endpoint with the app's credentials.
async fn token_request(http: &reqwest::Client, cfg: &Config, body: String) -> Result<Value> {
    let resp = http
        .post("https://accounts.spotify.com/api/token")
        .basic_auth(&cfg.spotify.client_id, Some(&cfg.spotify.client_secret))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("contacting Spotify")?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_default();
    if v.get("access_token").and_then(|t| t.as_str()).is_none() {
        bail!("Spotify token request failed ({status}): {}", v.get("error_description").or(v.get("error")).unwrap_or(&Value::Null));
    }
    Ok(v)
}

fn save_refresh_token(v: &Value) -> Result<()> {
    if let Some(rt) = v.get("refresh_token").and_then(|t| t.as_str()) {
        let path = token_path();
        std::fs::write(&path, serde_json::json!({ "refresh_token": rt }).to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

struct Spotify {
    http: reqwest::Client,
    token: String,
    /// Logged in as the user (needed for playlists and liked songs), not just the app.
    user: bool,
}

impl Spotify {
    async fn connect(cfg: &Config) -> Result<Self> {
        check_app(cfg)?;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let saved = std::fs::read_to_string(token_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if let Some(rt) = saved.as_ref().and_then(|v| v.get("refresh_token")).and_then(|t| t.as_str()) {
            match token_request(&http, cfg, format!("grant_type=refresh_token&refresh_token={}", enc(rt))).await {
                Ok(v) => {
                    save_refresh_token(&v)?; // Spotify may rotate it
                    let token = v["access_token"].as_str().unwrap_or_default().to_string();
                    return Ok(Self { http, token, user: true });
                }
                Err(e) => eprintln!("Spotify login expired ({e}); run `vibeseek spotify login` again"),
            }
        }
        let v = token_request(&http, cfg, "grant_type=client_credentials".into()).await?;
        let token = v["access_token"].as_str().unwrap_or_default().to_string();
        Ok(Self { http, token, user: false })
    }

    fn need_user(&self, what: &str) -> Result<()> {
        if !self.user {
            bail!("Spotify only shows {what} to logged-in users: run `vibeseek spotify login` once");
        }
        Ok(())
    }

    async fn get(&self, url: &str) -> Result<Value> {
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            let msg = v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("");
            if status == reqwest::StatusCode::NOT_FOUND {
                bail!("Spotify says not found — is it private, or a Spotify-made mix? ({msg})");
            }
            bail!("Spotify API {status}: {msg}");
        }
        Ok(v)
    }

    /// Follow `next` links, collecting `items`.
    async fn paged(&self, first: String) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut next = Some(first);
        while let Some(url) = next {
            let page = self.get(&url).await?;
            if let Some(items) = page.get("items").and_then(|i| i.as_array()) {
                out.extend(items.iter().cloned());
            }
            next = page.get("next").and_then(|n| n.as_str()).map(str::to_string);
        }
        Ok(out)
    }
}

/// `vibeseek spotify login`: authorize in the browser, catch the redirect on localhost, and keep
/// the refresh token so later runs are logged in automatically.
pub async fn login(cfg: &Config) -> Result<()> {
    check_app(cfg)?;
    let redirect = cfg.spotify.redirect_uri.clone();
    let addr = redirect
        .strip_prefix("http://")
        .and_then(|r| r.split('/').next())
        .context("spotify.redirect_uri must look like http://127.0.0.1:8888/callback")?
        .to_string();
    let listener = std::net::TcpListener::bind(&addr)
        .with_context(|| format!("can't listen on {addr} for the Spotify login redirect (is something else using it?)"))?;
    let state = uuid::Uuid::new_v4().simple().to_string();
    let url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&state={state}",
        enc(&cfg.spotify.client_id),
        enc(&redirect),
        enc(SCOPES)
    );
    println!("Opening your browser to log in to Spotify. If it doesn't open, visit:\n\n  {url}\n");
    let _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let wait = tokio::task::spawn_blocking(move || wait_for_code(listener, &state));
    let code = tokio::time::timeout(Duration::from_secs(300), wait)
        .await
        .context("timed out waiting for the Spotify login (5 minutes)")???;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let v = token_request(&http, cfg, format!("grant_type=authorization_code&code={}&redirect_uri={}", enc(&code), enc(&redirect))).await?;
    save_refresh_token(&v)?;
    let me: Value = http
        .get("https://api.spotify.com/v1/me")
        .bearer_auth(v["access_token"].as_str().unwrap_or_default())
        .send()
        .await?
        .json()
        .await
        .unwrap_or_default();
    let who = me.get("display_name").and_then(|n| n.as_str()).or(me.get("id").and_then(|n| n.as_str())).unwrap_or("you");
    println!("logged in to Spotify as {who}; playlists and `spotify-likes` work now");
    Ok(())
}

pub fn logout() -> Result<()> {
    match std::fs::remove_file(token_path()) {
        Ok(()) => println!("logged out of Spotify"),
        Err(_) => println!("wasn't logged in"),
    }
    Ok(())
}

/// Accept connections on the redirect address until Spotify sends the code.
fn wait_for_code(listener: std::net::TcpListener, state: &str) -> Result<String> {
    use std::io::{BufRead, BufReader, Write};
    loop {
        let (mut stream, _) = listener.accept()?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        // "GET /callback?code=...&state=... HTTP/1.1"
        let query = line.split_whitespace().nth(1).and_then(|p| p.split_once('?')).map(|(_, q)| q).unwrap_or("");
        let param = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(str::to_string);
        let reply = |stream: &mut std::net::TcpStream, msg: &str| {
            let body = format!("<html><body style=\"font-family:sans-serif\"><h3>{msg}</h3></body></html>");
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        };
        if let Some(err) = param("error") {
            reply(&mut stream, "Spotify login was cancelled. You can close this tab.");
            bail!("Spotify login failed: {err}");
        }
        match (param("code"), param("state")) {
            (Some(code), Some(s)) if s == state => {
                reply(&mut stream, "vibeseek is logged in to Spotify. You can close this tab.");
                return Ok(code);
            }
            _ => reply(&mut stream, "Waiting for Spotify…"), // favicon and other stray requests
        }
    }
}

fn spotify_row(track: &Value, album_name: Option<&str>) -> Option<Row> {
    if track.get("type").and_then(|t| t.as_str()).unwrap_or("track") != "track" {
        return None; // podcast episodes in playlists
    }
    let title = track.get("name")?.as_str()?.to_string();
    let artist = track
        .get("artists")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.get("name").and_then(|n| n.as_str())).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let album = album_name
        .map(str::to_string)
        .or_else(|| track.pointer("/album/name").and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_default();
    let length = track.get("duration_ms").and_then(|d| d.as_u64()).map(|ms| ((ms + 500) / 1000) as u32);
    Some(Row { line: 0, artist, title, album, length })
}

async fn spotify(cfg: &Config, kind: &str, id: &str) -> Result<Playlist> {
    let sp = Spotify::connect(cfg).await?;
    let api = "https://api.spotify.com/v1";
    let (name, rows) = match kind {
        "playlist" => {
            let meta = sp.get(&format!("{api}/playlists/{id}?fields=name")).await?;
            let name = meta.get("name").and_then(|n| n.as_str()).unwrap_or("spotify playlist").to_string();
            sp.need_user("playlist tracks")?;
            // Newer API: /items (entries under "item"); older: /tracks (under "track").
            let items = match sp.paged(format!("{api}/playlists/{id}/items?limit=50&additional_types=track")).await {
                Ok(i) => i,
                Err(_) => sp.paged(format!("{api}/playlists/{id}/tracks?limit=100&additional_types=track")).await?,
            };
            let rows = items.iter().filter_map(|it| it.get("item").or(it.get("track")).and_then(|t| spotify_row(t, None))).collect();
            (name, rows)
        }
        "liked" => {
            sp.need_user("your Liked Songs")?;
            let items = sp.paged(format!("{api}/me/tracks?limit=50")).await?;
            let rows = items.iter().filter_map(|it| it.get("track").and_then(|t| spotify_row(t, None))).collect();
            ("Spotify Liked Songs".to_string(), rows)
        }
        "album" => {
            let meta = sp.get(&format!("{api}/albums/{id}")).await?;
            let album = meta.get("name").and_then(|n| n.as_str()).unwrap_or("album").to_string();
            let artist = meta.pointer("/artists/0/name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let tracks = sp.paged(format!("{api}/albums/{id}/tracks?limit=50")).await?;
            let rows = tracks.iter().filter_map(|t| spotify_row(t, Some(&album))).collect();
            let name = if artist.is_empty() { album } else { format!("{artist} - {album}") };
            (name, rows)
        }
        _ => {
            let t = sp.get(&format!("{api}/tracks/{id}")).await?;
            let row = spotify_row(&t, None).context("not a music track")?;
            (format!("{} - {}", row.artist, row.title), vec![row])
        }
    };
    Ok(Playlist { name, rows: number(rows) })
}

// ---------- YouTube ----------

fn youtube(url: &str) -> Result<Playlist> {
    let out = std::process::Command::new("yt-dlp")
        .args(["--flat-playlist", "-J", "--no-warnings", "--ignore-errors", url])
        .output()
        .context("running yt-dlp (install it with `pacman -S yt-dlp`)")?;
    let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
        format!("yt-dlp couldn't read that link: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""))
    })?;
    let name = v.get("title").and_then(|t| t.as_str()).unwrap_or("youtube").to_string();
    let entries: Vec<Value> = match v.get("entries").and_then(|e| e.as_array()) {
        Some(e) => e.clone(),
        None => vec![v.clone()], // a single video
    };
    let rows = entries.iter().filter_map(youtube_row).collect();
    Ok(Playlist { name, rows: number(rows) })
}

fn youtube_row(e: &Value) -> Option<Row> {
    let s = |k: &str| e.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let raw_title = s("title")?;
    if raw_title == "[Deleted video]" || raw_title == "[Private video]" {
        return None;
    }
    let length = e.get("duration").and_then(|d| d.as_f64()).map(|d| d.round() as u32);
    // YouTube Music sometimes provides proper metadata.
    if let (Some(track), Some(artist)) = (s("track"), s("artist").or_else(|| s("creator"))) {
        return Some(Row { line: 0, artist, title: track, album: s("album").unwrap_or_default(), length });
    }
    let channel = s("channel").or_else(|| s("uploader")).unwrap_or_default();
    let (artist, title) = split_video_title(&raw_title, &channel);
    Some(Row { line: 0, artist, title, album: String::new(), length })
}

/// "Artist - Song (Official Video) [4K]" → ("Artist", "Song"). Falls back to the channel name
/// ("Artist - Topic", "ArtistVEVO") for the artist.
pub fn split_video_title(title: &str, channel: &str) -> (String, String) {
    let mut t = title.split(" | ").next().unwrap_or(title).to_string();
    for (open, close) in [('(', ')'), ('[', ']'), ('【', '】')] {
        t = strip_noise_groups(&t, open, close);
    }
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    for sep in [" - ", " – ", " — ", " -- "] {
        if let Some((a, b)) = t.split_once(sep) {
            return (a.trim().to_string(), b.trim().trim_matches('"').to_string());
        }
    }
    let artist = channel
        .trim_end_matches(" - Topic")
        .trim_end_matches("VEVO")
        .trim_end_matches(" Official")
        .trim()
        .to_string();
    (artist, t.trim_matches('"').to_string())
}

/// Remove bracketed groups like "(Official Video)" or "[4K]", keeping "(Live at Wembley)".
fn strip_noise_groups(s: &str, open: char, close: char) -> String {
    const NOISE: &[&str] = &[
        "official", "video", "audio", "lyric", "lyrics", "visualizer", "visualiser", "hd", "hq", "4k", "mv", "m/v",
        "explicit", "remaster", "remastered",
    ];
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find(open) {
        let Some(len) = rest[a..].find(close) else { break };
        let b = a + len + close.len_utf8(); // end of the group, exclusive
        let inner = rest[a + open.len_utf8()..a + len].to_lowercase();
        let noise = inner.split(|c: char| !c.is_alphanumeric() && c != '/').any(|w| NOISE.contains(&w));
        out.push_str(&rest[..a]);
        if !noise {
            out.push_str(&rest[a..b]);
        }
        rest = &rest[b..];
    }
    out.push_str(rest);
    out
}

fn number(mut rows: Vec<Row>) -> Vec<Row> {
    for (i, r) in rows.iter_mut().enumerate() {
        r.line = i + 1;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_links() {
        assert_eq!(spotify_ref("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc"), Some(("playlist", "37i9dQZF1DXcBWIGoYBM5M".into())));
        assert_eq!(spotify_ref("https://open.spotify.com/intl-de/album/4aawyAB9vmqN3uQ7FjRGTy"), Some(("album", "4aawyAB9vmqN3uQ7FjRGTy".into())));
        assert_eq!(spotify_ref("spotify:track:1YQWosTIljIvxAgHWTp7KP"), Some(("track", "1YQWosTIljIvxAgHWTp7KP".into())));
        assert_eq!(spotify_ref("https://www.youtube.com/playlist?list=x"), None);
        assert_eq!(spotify_ref("spotify-likes"), Some(("liked", String::new())));
    }

    #[test]
    fn video_titles() {
        let s = |t: &str, c: &str| split_video_title(t, c);
        assert_eq!(s("Deftones - Change (In the House of Flies) [Official Music Video]", "Deftones"), ("Deftones".into(), "Change (In the House of Flies)".into()));
        assert_eq!(s("Take Five", "The Dave Brubeck Quartet - Topic"), ("The Dave Brubeck Quartet".into(), "Take Five".into()));
        assert_eq!(s("Korn - Blind (Official HD Video)", "KornVEVO"), ("Korn".into(), "Blind".into()));
        assert_eq!(s("Weezer - Buddy Holly (Official Music Video) | Weezer", "weezer"), ("Weezer".into(), "Buddy Holly".into()));
        assert_eq!(s("Song (Live at Wembley)", "ArtistVEVO"), ("Artist".into(), "Song (Live at Wembley)".into()));
    }
}
