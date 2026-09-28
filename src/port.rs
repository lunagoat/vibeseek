//! Keeping slskd reachable: the VPN's forwarded port (ProtonVPN NAT-PMP) when the VPN is up,
//! otherwise a UPnP mapping on the home router (what Nicotine+ does by default).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::slskdcfg;

/// Run a command, killing it after `secs` (natpmpc retries forever when the VPN is down,
/// and UPnP discovery can stall on flaky routers).
fn run_timeout(cmd: &str, args: &[&str], secs: u64) -> Result<Output> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {cmd}"))?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{cmd} timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(child.wait_with_output()?)
}

/// Is the VPN's network interface present?
pub fn vpn_up(cfg: &Config) -> bool {
    Path::new("/sys/class/net").join(&cfg.port.vpn_interface).exists()
}

/// Ask the VPN gateway which public TCP port is forwarded to us.
/// Uses `natpmpc`, the same tool ProtonVPN documents for port forwarding.
pub fn detect(gateway: &str) -> Result<u16> {
    let out = run_timeout("natpmpc", &["-a", "1", "0", "tcp", "60", "-g", gateway], 6)
        .map_err(|_| anyhow::anyhow!("VPN gateway {gateway} didn't answer (is the VPN connected?)"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // "Mapped public port 40649 protocol TCP to local port 0 lifetime 60"
        if let Some(rest) = line.trim().strip_prefix("Mapped public port ") {
            if let Some(num) = rest.split_whitespace().next() {
                return num.parse().context("parsing natpmpc output");
            }
        }
    }
    bail!("VPN gateway {gateway} didn't return a forwarded port (is port forwarding on?)")
}

/// Open `port` on the home router, forwarded to this machine.
pub fn upnp_map(port: u16) -> Result<()> {
    let p = port.to_string();
    let out = run_timeout("upnpc", &["-e", "vibeseek slskd", "-r", &p, "TCP"], 20)?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("is redirected to internal") {
        return Ok(());
    }
    let why = text.lines().find(|l| l.contains("failed") || l.contains("No IGD")).unwrap_or("no UPnP router answered");
    bail!("UPnP mapping of port {port} failed: {}", why.trim())
}

/// Close the router port again (harmless if it isn't open).
pub fn upnp_unmap(port: u16) -> Result<()> {
    run_timeout("upnpc", &["-d", &port.to_string(), "TCP"], 20).map(|_| ())
}

pub enum SyncResult {
    Unchanged(u16),
    Changed { from: u16, to: u16 },
}

/// Set slskd's listen port. slskd hot-reloads it, no restart needed.
pub fn apply(cfg: &Config, port: u16) -> Result<SyncResult> {
    let yml = cfg.slskd_yml();
    let current = slskdcfg::listen_port(&yml)?;
    if current == port {
        return Ok(SyncResult::Unchanged(port));
    }
    slskdcfg::set_listen_port(&yml, port)?;
    Ok(SyncResult::Changed { from: current, to: port })
}

/// How peers reach us.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    Vpn,
    Upnp,
}

impl Route {
    pub fn describe(self) -> &'static str {
        match self {
            Route::Vpn => "VPN port forwarding",
            Route::Upnp => "router port (UPnP)",
        }
    }
}

/// Remembered between agent cycles.
pub struct PortState {
    pub route: Option<Route>,
    upnp_mapped: bool,
    last_map: Instant,
    /// A mapping from a previous run may still be open; close it once the VPN is seen.
    cleaned: bool,
}

impl Default for PortState {
    fn default() -> Self {
        Self { route: None, upnp_mapped: false, last_map: Instant::now(), cleaned: false }
    }
}

pub struct Step {
    pub route: Route,
    pub result: SyncResult,
    /// Something worth logging (a router port opened/closed).
    pub note: Option<String>,
}

/// One sync pass: use the VPN's port when the VPN is up, otherwise open a router port.
pub fn step(cfg: &Config, st: &mut PortState) -> Result<Step> {
    let upnp_port = cfg.port.upnp_port;
    if vpn_up(cfg) {
        let port = detect(&cfg.port.gateway)?;
        let mut note = None;
        if st.upnp_mapped || !st.cleaned {
            let was = st.upnp_mapped;
            let _ = upnp_unmap(upnp_port);
            st.upnp_mapped = false;
            st.cleaned = true;
            if was {
                note = Some(format!("closed router port {upnp_port}"));
            }
        }
        let result = apply(cfg, port)?;
        st.route = Some(Route::Vpn);
        return Ok(Step { route: Route::Vpn, result, note });
    }
    if !cfg.port.upnp {
        bail!("VPN is down and UPnP fallback is off (port.upnp in config.toml)");
    }
    let mut note = None;
    // Re-map every 10 minutes in case the router rebooted or our LAN address changed.
    if !st.upnp_mapped || st.last_map.elapsed() > Duration::from_secs(600) {
        upnp_map(upnp_port)?;
        if !st.upnp_mapped {
            note = Some(format!("opened router port {upnp_port} via UPnP"));
        }
        st.upnp_mapped = true;
        st.cleaned = true;
        st.last_map = Instant::now();
    }
    let result = apply(cfg, upnp_port)?;
    st.route = Some(Route::Upnp);
    Ok(Step { route: Route::Upnp, result, note })
}

/// The page SoulseekQT's "Check ports" opens. It tests whichever IP makes the request, which is
/// the same route slskd's traffic takes (VPN or home connection).
pub fn test_url(port: u16) -> String {
    format!("http://tools.slsknet.org/porttest.php?port={port}")
}

pub struct PortTest {
    pub open: bool,
    /// The service's verdict, e.g. "IP: 1.2.3.4 Port: 41193/tcp open. Your router and …"
    pub message: String,
}

/// Ask Soulseek's port tester whether peers can reach `port`.
pub async fn test(port: u16) -> Result<PortTest> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let html = http.get(test_url(port)).send().await.context("contacting the Soulseek port tester")?.text().await?;
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let start = text.find("IP:").context("unexpected reply from the port tester")?;
    let rest = &text[start..];
    let end = rest.find("Don't forget").unwrap_or(rest.len());
    let message = rest[..end].trim().to_string();
    let open = message.contains("tcp open");
    Ok(PortTest { open, message })
}

/// Result of the agent's last reachability check (shown by `vibeseek status`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Health {
    pub checked_at: chrono::DateTime<chrono::Utc>,
    pub port: u16,
    pub route: String,
    pub open: bool,
    pub message: String,
    /// When the current open/closed state began.
    pub since: chrono::DateTime<chrono::Utc>,
}

fn health_path() -> std::path::PathBuf {
    crate::config::data_dir().join("port_health.json")
}

pub fn load_health() -> Option<Health> {
    std::fs::read(health_path()).ok().and_then(|d| serde_json::from_slice(&d).ok())
}

pub fn save_health(h: &Health) {
    if let Ok(d) = serde_json::to_vec_pretty(h) {
        let _ = std::fs::write(health_path(), d);
    }
}

/// Desktop notification (best effort; needs a notification daemon like dunst).
pub fn notify(title: &str, body: &str) {
    let _ = Command::new("notify-send").args(["-a", "vibeseek", title, body]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}
