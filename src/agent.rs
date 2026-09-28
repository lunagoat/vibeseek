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
    let mut last_port_err_logged = Instant::now();
    let mut port_err_since: Option<Instant> = None;
    let mut warned_no_vpn_port = false;
    let mut port_state = PortState::default();
    // First reachability test a minute after start, then every `check_minutes`.
    let check_every = Duration::from_secs(cfg.port.check_minutes.max(1) * 60);
    let mut next_health = Instant::now() + Duration::from_secs(60);
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
                    if port_err_since.take().is_some() && !last_port_err.is_empty() {
                        eprintln!("port sync working again");
                    }
                    last_port_err.clear();
                    warned_no_vpn_port = false;
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
                        // Verify the new route actually works once things settle.
                        next_health = next_health.min(Instant::now() + Duration::from_secs(60));
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    let since = *port_err_since.get_or_insert_with(Instant::now);
                    // Log each new error, and repeat it hourly so a long outage is visible.
                    if msg != last_port_err || last_port_err_logged.elapsed() > Duration::from_secs(3600) {
                        let mins = since.elapsed().as_secs() / 60;
                        if msg == last_port_err {
                            eprintln!("port sync still failing ({mins} min): {msg}");
                        } else {
                            eprintln!("port sync: {msg}");
                        }
                        last_port_err = msg.clone();
                        last_port_err_logged = Instant::now();
                    }
                    // VPN up but no forwarded port: slskd keeps advertising a dead port and
                    // nobody can reach you. Only reconnecting the VPN fixes that.
                    if msg.contains("didn't return a forwarded port")
                        && since.elapsed() > Duration::from_secs(600)
                        && !warned_no_vpn_port
                    {
                        warned_no_vpn_port = true;
                        if cfg.port.notify {
                            port::notify(
                                "Soulseek: VPN isn't forwarding a port",
                                "Nobody can connect to you, so uploads stop. Reconnect ProtonVPN (with port forwarding on).",
                            );
                        }
                    }
                }
            }
        }
        if cfg.port.check_minutes > 0 && Instant::now() >= next_health {
            next_health = Instant::now() + check_every;
            if let Some(retry_soon) = check_reachability(cfg, &client, &port_state).await {
                if retry_soon {
                    next_health = Instant::now() + Duration::from_secs(120);
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

/// Test the listen port with Soulseek's port tester and record the result. On a change,
/// log it and notify; when unreachable, re-sync the port and reconnect slskd.
/// Returns Some(true) when a quick re-check is wanted, None if the test couldn't run.
async fn check_reachability(cfg: &Config, client: &Client, st: &PortState) -> Option<bool> {
    // Only meaningful while logged in.
    if !client.application().await.ok()?.server.is_logged_in {
        return None;
    }
    let port = crate::slskdcfg::listen_port(&cfg.slskd_yml()).ok()?;
    let test = match port::test(port).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("port test couldn't run: {e:#}");
            return None;
        }
    };
    let route = st.route.map(|r| r.describe()).unwrap_or("manual port").to_string();
    let now = chrono::Utc::now();
    let prev = port::load_health();
    let changed = prev.as_ref().map(|p| p.open != test.open || p.port != port).unwrap_or(true);
    let since = match &prev {
        Some(p) if !changed => p.since,
        _ => now,
    };
    port::save_health(&port::Health { checked_at: now, port, route: route.clone(), open: test.open, message: test.message.clone(), since });

    if test.open {
        if changed {
            eprintln!("port {port} reachable ({route})");
            if prev.map(|p| !p.open).unwrap_or(false) && cfg.port.notify {
                port::notify("Soulseek: reachable again", &format!("Port {port} is open ({route})."));
            }
        }
        return Some(false);
    }
    eprintln!("port {port} NOT reachable ({route}): {}", test.message);
    if changed && cfg.port.notify {
        port::notify(
            "Soulseek: you're unreachable",
            &format!("Port {port} is closed ({route}). Peers can't connect, so uploads stop. vibeseek is trying to fix it; if you're on the VPN, reconnect it."),
        );
    }
    // Remedy attempt: re-request the port mapping and re-announce to the server.
    let c = cfg.clone();
    let _ = tokio::task::spawn_blocking(move || port::step(&c, &mut PortState::default())).await;
    let _ = client.reconnect().await;
    Some(true)
}
