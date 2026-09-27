//! vibeseek configuration (~/.config/vibeseek/config.toml) and well-known paths.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::quality::{Filter, Prefs};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub slskd: SlskdConfig,
    pub port: PortConfig,
    pub search: SearchConfig,
    pub csv: CsvConfig,
    pub spotify: SpotifyConfig,
    /// Preference ranking applied to every search (never hides results).
    pub prefs: Prefs,
    /// Named filter presets; `search.default_preset` picks the default.
    pub presets: BTreeMap<String, Filter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlskdConfig {
    pub url: String,
    /// API key. If empty, it's read from the slskd.yml `web.authentication.api_keys.vibeseek`.
    pub api_key: String,
    /// Path to slskd.yml (used for port + ban management).
    pub config_path: String,
    /// slskd's configured downloads directory.
    pub downloads_dir: String,
    /// systemd user unit name.
    pub service: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PortConfig {
    /// Ask the VPN gateway (NAT-PMP) for the forwarded port and keep slskd in sync.
    pub auto: bool,
    pub gateway: String,
    /// Network interface that exists while the VPN is connected.
    pub vpn_interface: String,
    /// When the VPN is down, open a port on the home router via UPnP (like Nicotine+).
    pub upnp: bool,
    pub upnp_port: u16,
}

/// A Spotify developer app (https://developer.spotify.com/dashboard), for playlist links.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Must match a Redirect URI registered on the app.
    pub redirect_uri: String,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self { client_id: String::new(), client_secret: String::new(), redirect_uri: "http://127.0.0.1:8888/callback".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub timeout_secs: u64,
    pub response_limit: u32,
    pub file_limit: u32,
    pub default_preset: String,
    /// Default download folder (empty = slskd's downloads dir, organized by source folder).
    pub default_output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CsvConfig {
    /// Concurrent searches while processing a CSV.
    pub concurrency: usize,
    /// Soulseek throttles searching; stay below N searches per window.
    pub searches_per_window: usize,
    pub window_secs: u64,
    /// Allowed track length difference, seconds.
    pub length_tolerance: u32,
    /// Try up to N alternative sources when a download fails.
    pub max_attempts: usize,
    pub preset: String,
    /// Preset used as fallback when nothing matches `preset` (empty = none).
    pub fallback_preset: String,
}

impl Default for Config {
    fn default() -> Self {
        let mut presets = BTreeMap::new();
        presets.insert(
            "lossless".into(),
            Filter {
                formats: vec!["flac".into()],
                min_bitdepth: Some(16),
                min_samplerate: Some(44100),
                ..Default::default()
            },
        );
        presets.insert(
            "lossy-ok".into(),
            Filter {
                formats: ["mp3", "flac", "ogg", "m4a", "opus", "wav", "aac", "alac", "aiff"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                min_bitrate: Some(192),
                ..Default::default()
            },
        );
        presets.insert("any".into(), Filter::default());
        Self {
            slskd: SlskdConfig::default(),
            port: PortConfig::default(),
            search: SearchConfig::default(),
            csv: CsvConfig::default(),
            spotify: SpotifyConfig::default(),
            prefs: Prefs::default(),
            presets,
        }
    }
}

impl Default for SlskdConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:5030".into(),
            api_key: String::new(),
            config_path: "~/.local/share/slskd/slskd.yml".into(),
            downloads_dir: "~/Music/downloads".into(),
            service: "slskd.service".into(),
        }
    }
}

impl Default for PortConfig {
    fn default() -> Self {
        Self { auto: true, gateway: "10.2.0.1".into(), vpn_interface: "proton0".into(), upnp: true, upnp_port: 50300 }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 12,
            response_limit: 500,
            file_limit: 20000,
            default_preset: "lossless".into(),
            default_output: String::new(),
        }
    }
}

impl Default for CsvConfig {
    fn default() -> Self {
        Self {
            concurrency: 2,
            searches_per_window: 34,
            window_secs: 220,
            length_tolerance: 3,
            max_attempts: 3,
            preset: "lossless".into(),
            fallback_preset: String::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_file();
        let mut cfg: Config = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            let cfg = Config::default();
            cfg.save()?;
            cfg
        };
        // Make sure built-in presets exist even if the user trimmed the file.
        for (k, v) in Config::default().presets {
            cfg.presets.entry(k).or_insert(v);
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let header = "# vibeseek configuration. See `vibeseek --help`.\n\n";
        std::fs::write(&path, format!("{header}{}", toml::to_string_pretty(self)?))?;
        Ok(())
    }

    pub fn preset(&self, name: &str) -> Result<Filter> {
        self.presets.get(name).cloned().with_context(|| {
            let names: Vec<_> = self.presets.keys().cloned().collect();
            format!("unknown preset '{name}' (available: {})", names.join(", "))
        })
    }

    pub fn slskd_yml(&self) -> PathBuf {
        expand(&self.slskd.config_path)
    }

    pub fn downloads_dir(&self) -> PathBuf {
        expand(&self.slskd.downloads_dir)
    }

    /// API key from config, or discovered from slskd.yml.
    pub fn api_key(&self) -> Result<String> {
        if !self.slskd.api_key.is_empty() {
            return Ok(self.slskd.api_key.clone());
        }
        crate::slskdcfg::read_api_key(&self.slskd_yml())
            .context("no API key configured; set slskd.api_key in ~/.config/vibeseek/config.toml")
    }
}

pub fn config_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home().join(".config")).join("vibeseek/config.toml")
}

pub fn data_dir() -> PathBuf {
    let d = dirs::data_dir().unwrap_or_else(|| home().join(".local/share")).join("vibeseek");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn cache_dir() -> PathBuf {
    let d = dirs::cache_dir().unwrap_or_else(|| home().join(".cache")).join("vibeseek");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand a leading `~` and make relative paths absolute.
pub fn expand(p: &str) -> PathBuf {
    let path = if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    };
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map(|c| c.join(&path)).unwrap_or(path)
    }
}

/// Pretty-print a path with `~` for the home dir.
pub fn tilde(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}
