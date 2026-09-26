//! Forwarded-port detection (ProtonVPN NAT-PMP) and syncing it into slskd.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::config::Config;
use crate::slskdcfg;

/// Ask the VPN gateway which public TCP port is forwarded to us.
/// Uses `natpmpc`, the same tool ProtonVPN documents for port forwarding.
pub fn detect(gateway: &str) -> Result<u16> {
    let out = Command::new("natpmpc")
        .args(["-a", "1", "0", "tcp", "60", "-g", gateway])
        .output()
        .context("running natpmpc (install libnatpmp)")?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // "Mapped public port 40649 protocol TCP to local port 0 lifetime 60"
        if let Some(rest) = line.trim().strip_prefix("Mapped public port ") {
            if let Some(num) = rest.split_whitespace().next() {
                return num.parse().context("parsing natpmpc output");
            }
        }
    }
    bail!("VPN gateway {gateway} didn't return a forwarded port (is the VPN connected with port forwarding on?)")
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

pub fn sync(cfg: &Config) -> Result<SyncResult> {
    let port = detect(&cfg.port.gateway)?;
    apply(cfg, port)
}
