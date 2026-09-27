//! systemd --user wrappers for slskd and the vibeseek agent.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::DaemonAction;

pub const AGENT_UNIT: &str = "vibeseek-agent.service";

pub fn systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl").arg("--user").args(args).status().context("running systemctl")?;
    if !st.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

pub fn is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn run(cfg: &Config, action: DaemonAction) -> Result<()> {
    let unit = cfg.slskd.service.as_str();
    match action {
        DaemonAction::Start => {
            if cfg.port.auto {
                // Make sure slskd comes up on the right port.
                if let Ok(step) = crate::port::step(cfg, &mut crate::port::PortState::default()) {
                    if let crate::port::SyncResult::Changed { from, to } = step.result {
                        println!("port {from} → {to}");
                    }
                }
            }
            systemctl(&["start", unit])?;
            println!("slskd started");
        }
        DaemonAction::Stop => {
            systemctl(&["stop", unit])?;
            println!("slskd stopped (Soulseek login released; sockseek/Nicotine+ can use it now)");
        }
        DaemonAction::Restart => {
            systemctl(&["restart", unit])?;
            println!("slskd restarted");
        }
        DaemonAction::Status => {
            let _ = Command::new("systemctl").args(["--user", "status", "--no-pager", "-n", "5", unit]).status();
        }
        DaemonAction::Logs => {
            let _ = Command::new("journalctl").args(["--user", "-u", unit, "-f", "-n", "50"]).status();
        }
    }
    Ok(())
}
