//! The background agent: keeps slskd's port in sync with the VPN, records upload history
//! (slskd forgets finished transfers), and moves staged downloads into their target folders.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use crate::api::Client;
use crate::config::Config;
use crate::daemon::{self, AGENT_UNIT};
use crate::history::History;
use crate::port::{self, SyncResult};
use crate::AgentAction;

pub async fn run(cfg: &Config, action: Option<AgentAction>) -> Result<()> {
    match action.unwrap_or(AgentAction::Run) {
        AgentAction::Run => run_loop(cfg).await,
        AgentAction::Install => install(),
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

fn install() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let unit = format!(
        "[Unit]\n\
         Description=vibeseek agent (port sync, upload history, download mover)\n\
         After=slskd.service\n\
         Wants=slskd.service\n\n\
         [Service]\n\
         ExecStart={} agent run\n\
         Restart=always\n\
         RestartSec=15\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    );
    let path = unit_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit)?;
    daemon::systemctl(&["daemon-reload"])?;
    daemon::systemctl(&["enable", "--now", AGENT_UNIT])?;
    println!("agent installed and running ({})", crate::config::tilde(&path));
    Ok(())
}

async fn run_loop(cfg: &Config) -> Result<()> {
    let history = History::open().context("opening history db")?;
    let mut last_port_check = Instant::now() - Duration::from_secs(3600);
    let mut last_port_err = String::new();
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
        if cfg.port.auto && last_port_check.elapsed() >= Duration::from_secs(45) {
            last_port_check = Instant::now();
            // natpmpc blocks for a moment; keep it off the async threads.
            let c = cfg.clone();
            match tokio::task::spawn_blocking(move || port::sync(&c)).await? {
                Ok(SyncResult::Changed { from, to }) => eprintln!("forwarded port changed {from} → {to}; slskd updated"),
                Ok(SyncResult::Unchanged(_)) => last_port_err.clear(),
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
