//! The background agent: keeps slskd's port in sync with the VPN, records upload history
//! (slskd forgets finished transfers), and moves staged downloads into their target folders.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use crate::api::Client;
use crate::config::Config;
use crate::daemon::{self, AGENT_UNIT};
use crate::history::History;
use crate::port::{self, PortState, SyncResult};
use crate::AgentAction;

pub async fn run(cfg: &Config, action: Option<AgentAction>) -> Result<()> {
    match action.unwrap_or(AgentAction::Run) {
        AgentAction::Run => run_loop(cfg).await,
        AgentAction::Install => install(cfg),
        AgentAction::Uninstall => {
            let _ = daemon::systemctl(&["disable", "--now", AGENT_UNIT]);
            let path = unit_path();
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            daemon::systemctl(&["daemon-reload"])?;
            println!("agent removed");
            Ok(())
        }
    }
}

fn unit_path() -> std::path::PathBuf {
    crate::config::home().join(".config/systemd/user").join(AGENT_UNIT)
}

/// The agent's systemd unit, running `exe agent run` alongside `slskd_unit`.
pub fn unit_text(exe: &std::path::Path, slskd_unit: &str) -> String {
    format!(
        "[Unit]\n\
         Description=vibeseek agent (port sync, upload history, download mover)\n\
         After={slskd_unit}\n\
         Wants={slskd_unit}\n\n\
         [Service]\n\
         ExecStart=\"{}\" agent run\n\
         Restart=always\n\
         RestartSec=15\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    )
}

fn install(cfg: &Config) -> Result<()> {
    let exe = crate::setup::stable_exe()?;
    let path = unit_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit_text(&exe, &cfg.slskd.service))?;
    daemon::systemctl(&["daemon-reload"])?;
    daemon::systemctl(&["enable", "--now", AGENT_UNIT])?;
    println!("agent installed and running ({})", crate::config::tilde(&path));
    Ok(())
}

async fn run_loop(cfg: &Config) -> Result<()> {
    let history = History::open().context("opening history db")?;
    let mut last_port_check = Instant::now() - Duration::from_secs(3600);
    let mut last_port_err = String::new();
    let mut port_state = PortState::default();
    eprintln!("vibeseek agent running");
    loop {
        // Rebuilt each cycle so a changed API key in slskd.yml is picked up without a restart.
        let client = match Client::new(cfg) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("slskd config: {e:#}");
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
        };
        if let Err(e) = crate::slskdcfg::ensure_dirs(&cfg.slskd_yml()) {
            eprintln!("{e:#}");
        }
        if cfg.port.auto && last_port_check.elapsed() >= Duration::from_secs(20) {
            last_port_check = Instant::now();
            let prev_route = port_state.route;
            // natpmpc/upnpc block for a moment; keep them off the async threads.
            let c = cfg.clone();
            let (st, res) = tokio::task::spawn_blocking(move || {
                let mut st = port_state;
                let r = port::step(&c, &mut st);
                (st, r)
            })
            .await?;
            port_state = st;
            match res {
                Ok(step) => {
                    last_port_err.clear();
                    if let Some(note) = &step.note {
                        eprintln!("{note}");
                    }
                    let route_changed = prev_route.is_some() && prev_route != Some(step.route);
                    if route_changed {
                        eprintln!("now reachable through {}", step.route.describe());
                    }
                    let port_changed = matches!(step.result, SyncResult::Changed { .. });
                    if let SyncResult::Changed { from, to } = step.result {
                        eprintln!("listen port {from} → {to}; slskd updated");
                    }
                    // The old server connection may have died with the VPN without slskd noticing,
                    // and the server needs our new port: reconnect.
                    if route_changed || port_changed {
                        match client.reconnect().await {
                            Ok(()) => eprintln!("reconnected slskd to Soulseek"),
                            Err(e) => eprintln!("reconnect failed: {e:#}"),
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg != last_port_err {
                        eprintln!("port sync: {msg}");
                        last_port_err = msg;
                    }
                }
            }
        }
        match client.uploads().await {
            Ok(ups) => {
                if let Ok(n) = history.record(&ups) {
                    if n > 0 {
                        eprintln!("recorded {n} finished upload(s)");
                    }
                }
            }
            Err(e) => eprintln!("slskd: {e}"),
        }
        if let Ok(n) = crate::download::process_moves(&client).await {
            if n > 0 {
                eprintln!("moved {n} finished download(s) into place");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
