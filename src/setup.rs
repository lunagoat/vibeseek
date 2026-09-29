//! `vibeseek setup`: first-run wizard for a fresh machine. Downloads slskd, writes its config,
//! installs vibeseek into ~/.local/bin, and sets up the background services.

use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, home, Config};
use crate::daemon::{self, AGENT_UNIT};

/// Service name used by setup, so it can't collide with an slskd the user already runs.
pub const SLSKD_UNIT: &str = "vibeseek-slskd.service";

fn slskd_dir() -> PathBuf {
    config::data_dir().join("slskd")
}

fn unit_dir() -> PathBuf {
    home().join(".config/systemd/user")
}

/// Where vibeseek lives after setup (the AppImage copies itself here).
pub fn installed_exe() -> PathBuf {
    home().join(".local/bin/vibeseek")
}

/// A path to this program that stays valid after it exits (an AppImage runs from a temporary
/// mount, so use the .AppImage file itself, or the installed copy).
pub fn stable_exe() -> Result<PathBuf> {
    if installed_exe().exists() {
        return Ok(installed_exe());
    }
    if let Ok(p) = std::env::var("APPIMAGE") {
        return Ok(PathBuf::from(p));
    }
    Ok(std::env::current_exe()?.canonicalize()?)
}

pub fn is_set_up(cfg: &Config) -> bool {
    cfg.slskd_yml().exists()
}

// ---------- prompts ----------

fn ask(prompt: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{prompt}: ");
    } else {
        print!("{prompt} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("setup cancelled");
    }
    let line = line.trim();
    Ok(if line.is_empty() { default.to_string() } else { line.to_string() })
}

fn ask_yes(prompt: &str, default_yes: bool) -> Result<bool> {
    let a = ask(&format!("{prompt} ({})", if default_yes { "Y/n" } else { "y/N" }), "")?;
    Ok(match a.to_lowercase().as_str() {
        "" => default_yes,
        s => s.starts_with('y'),
    })
}

fn heading(s: &str) {
    println!("\n\x1b[1;36m{s}\x1b[0m");
}

fn random_key(len: usize) -> String {
    let mut s = String::new();
    while s.len() < len {
        s.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    s.truncate(len);
    s
}

fn have(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {cmd}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------- slskd download ----------

async fn install_slskd() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "linux-x64",
        "aarch64" => "linux-arm64",
        other => bail!("no slskd build for {other}"),
    };
    let http = reqwest::Client::builder().user_agent("vibeseek-setup").timeout(Duration::from_secs(300)).build()?;
    let rel: serde_json::Value = http
        .get("https://api.github.com/repos/slskd/slskd/releases/latest")
        .send()
        .await
        .context("contacting GitHub")?
        .json()
        .await?;
    let version = rel.get("tag_name").and_then(|t| t.as_str()).unwrap_or("?").to_string();
    let asset = rel
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| {
            a.iter().find(|x| {
                let n = x.get("name").and_then(|n| n.as_str()).unwrap_or("");
                n.ends_with(&format!("-{arch}.zip")) && !n.contains("musl")
            })
        })
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .with_context(|| format!("no {arch} download in slskd {version}"))?
        .to_string();
    print!("downloading slskd {version}… ");
    std::io::stdout().flush()?;
    let bytes = http.get(&asset).send().await?.error_for_status()?.bytes().await?;
    println!("{:.0} MB", bytes.len() as f64 / 1e6);

    let dir = slskd_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("reading slskd zip")?;
    zip.extract(&dir).context("unpacking slskd")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("slskd"), std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(version)
}

// ---------- config files ----------

fn yaml_map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(Value::String(k.into()), v);
    }
    Value::Mapping(m)
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

struct Answers {
    username: String,
    password: String,
    shares: Vec<PathBuf>,
    downloads: PathBuf,
    batch_root: PathBuf,
    listen_port: u16,
    /// Keep the port synced automatically (VPN / UPnP); false = user forwards it themselves.
    auto_port: bool,
}

fn write_slskd_yml(path: &Path, a: &Answers) -> Result<()> {
    let incomplete = home().join(".local/share/slskd/incomplete");
    std::fs::create_dir_all(&incomplete)?;
    std::fs::create_dir_all(&a.downloads)?;
    let v = yaml_map(vec![
        (
            "directories",
            yaml_map(vec![("downloads", s(&a.downloads.to_string_lossy())), ("incomplete", s(&incomplete.to_string_lossy()))]),
        ),
        (
            "shares",
            yaml_map(vec![
                ("directories", Value::Sequence(a.shares.iter().map(|p| s(&p.to_string_lossy())).collect())),
                ("filters", Value::Sequence(vec![s(r"\.ini$"), s("Thumbs.db$"), s(r"\.DS_Store$")])),
            ]),
        ),
        (
            "transfers",
            yaml_map(vec![
                ("download", yaml_map(vec![("destination", yaml_map(vec![("subdirectory", s("${SOURCE_DIRECTORY}")), ("exists", s("rename"))]))])),
                ("groups", yaml_map(vec![("blacklisted", yaml_map(vec![("members", Value::Sequence(vec![]))]))])),
            ]),
        ),
        // Clean up vibeseek's own searches; finished transfers are kept (slskd's default).
        ("retention", yaml_map(vec![("search", Value::Number(60.into()))])),
        (
            "web",
            yaml_map(vec![
                ("port", Value::Number(5030.into())),
                ("ip_address", s("127.0.0.1")),
                ("https", yaml_map(vec![("disabled", Value::Bool(true))])),
                (
                    "authentication",
                    yaml_map(vec![
                        ("username", s("vibeseek")),
                        ("password", s(&random_key(24))),
                        ("jwt", yaml_map(vec![("key", s(&random_key(40)))])),
                        (
                            "api_keys",
                            yaml_map(vec![(
                                "vibeseek",
                                yaml_map(vec![("key", s(&random_key(40))), ("role", s("administrator")), ("cidr", s("127.0.0.1/32,::1/128"))]),
                            )]),
                        ),
                    ]),
                ),
            ]),
        ),
        (
            "soulseek",
            yaml_map(vec![
                ("username", s(&a.username)),
                ("password", s(&a.password)),
                ("description", s("vibeseek / slskd user\n")),
                ("listen_port", Value::Number(a.listen_port.into())),
            ]),
        ),
    ]);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("# Written by `vibeseek setup` (edit freely; slskd reloads on change)\n{}", serde_yaml::to_string(&v)?))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn write_units(cfg: &Config, exe: &Path) -> Result<()> {
    let dir = unit_dir();
    std::fs::create_dir_all(&dir)?;
    let slskd = slskd_dir();
    let yml = cfg.slskd_yml();
    let app_dir = yml.parent().unwrap_or(Path::new("."));
    std::fs::write(
        dir.join(SLSKD_UNIT),
        format!(
            "[Unit]\nDescription=slskd Soulseek daemon (for vibeseek)\nAfter=network-online.target\n\n\
             [Service]\nType=simple\n\
             ExecStart=\"{bin}\" --app-dir \"{app}\" --config \"{yml}\" --no-logo\n\
             WorkingDirectory={dir}\n\
             Environment=DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1\n\
             Environment=DOTNET_BUNDLE_EXTRACT_BASE_DIR=%h/.cache/slskd\n\
             Restart=on-failure\nRestartSec=10\n\n\
             [Install]\nWantedBy=default.target\n",
            bin = slskd.join("slskd").display(),
            app = app_dir.display(),
            yml = yml.display(),
            dir = slskd.display(),
        ),
    )?;
    std::fs::write(dir.join(AGENT_UNIT), crate::agent::unit_text(exe, SLSKD_UNIT))?;
    Ok(())
}

fn install_self() -> Result<PathBuf> {
    let src = std::env::var("APPIMAGE").map(PathBuf::from).or_else(|_| std::env::current_exe())?;
    let dst = installed_exe();
    if src.canonicalize().ok() == dst.canonicalize().ok() {
        return Ok(dst);
    }
    std::fs::create_dir_all(dst.parent().unwrap())?;
    // Copy to a temp name first: the destination may be the running binary.
    let tmp = dst.with_extension("new");
    std::fs::copy(&src, &tmp).with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &dst)?;
    Ok(dst)
}

// ---------- the wizard ----------

pub async fn run(mut cfg: Config) -> Result<()> {
    println!("\x1b[1mvibeseek setup\x1b[0m — Soulseek in your terminal.");
    println!("This installs slskd (the Soulseek engine vibeseek drives) and sets everything up.\n");

    if is_set_up(&cfg) {
        println!("vibeseek is already set up ({}).", config::tilde(&cfg.slskd_yml()));
        if !ask_yes("Start over? This replaces your Soulseek login and shares", false)? {
            return Ok(());
        }
    }
    if !have("systemctl") {
        bail!("vibeseek needs systemd (systemctl) to run slskd in the background");
    }

    heading("1. Soulseek account");
    println!("New to Soulseek? Just pick a username and password: the account is created the first time you log in.");
    let username = loop {
        let u = ask("Username", "")?;
        if !u.is_empty() {
            break u;
        }
    };
    let password = loop {
        let p = rpassword::prompt_password("Password: ")?;
        if !p.is_empty() {
            break p;
        }
    };

    heading("2. Sharing");
    println!("Soulseek works by sharing: people who share nothing get low priority or get banned.");
    println!("Enter folders to share, one per line (empty line when done):");
    let mut shares = vec![];
    loop {
        let p = ask("  folder", "")?;
        if p.is_empty() {
            break;
        }
        let path = config::expand(&p);
        if path.is_dir() {
            shares.push(path);
        } else {
            println!("  \x1b[33mnot a folder: {}\x1b[0m", path.display());
        }
    }
    if shares.is_empty() {
        println!("  \x1b[33m(sharing nothing for now — add folders later under shares: in the slskd config)\x1b[0m");
    }

    heading("3. Where downloads go");
    let downloads = config::expand(&ask("Single downloads", "~/Music/soulseek")?);
    let batch_root = config::expand(&ask("CSV / playlist downloads (one folder each)", "~/Music/soulseek/playlists")?);

    heading("4. Letting people connect to you");
    println!("Other users must be able to reach you, or searches come back nearly empty.");
    println!("  1) Automatic: open a port on my router with UPnP (most home routers)");
    println!("  2) ProtonVPN with port forwarding (auto-detected when the VPN is on; UPnP otherwise)");
    println!("  3) I'll forward a port on my router myself");
    let (listen_port, auto_port) = match ask("Choose", "1")?.as_str() {
        "3" => {
            let p: u16 = ask("Port you forwarded (TCP)", "50300")?.parse().context("not a port number")?;
            (p, false)
        }
        _ => (50300, true),
    };
    if auto_port && !have("upnpc") {
        println!("  \x1b[33mupnpc isn't installed — install the 'miniupnpc' package so vibeseek can open the port\x1b[0m");
        println!("  \x1b[2m(apt install miniupnpc · pacman -S miniupnpc · dnf install miniupnpc)\x1b[0m");
    }

    let answers = Answers { username, password, shares, downloads, batch_root, listen_port, auto_port };

    heading("Installing");
    let version = install_slskd().await?;
    write_slskd_yml(&cfg.slskd_yml(), &answers)?;
    println!("wrote {}", config::tilde(&cfg.slskd_yml()));

    cfg.slskd.service = SLSKD_UNIT.into();
    cfg.slskd.downloads_dir = config::tilde(&answers.downloads);
    cfg.csv.output_root = config::tilde(&answers.batch_root);
    cfg.port.auto = answers.auto_port;
    cfg.port.upnp_port = answers.listen_port;
    cfg.save()?;
    println!("wrote {}", config::tilde(&config::config_file()));

    let exe = install_self()?;
    println!("installed {}", config::tilde(&exe));
    if !std::env::var("PATH").unwrap_or_default().split(':').any(|p| Path::new(p) == exe.parent().unwrap()) {
        println!("  \x1b[33m~/.local/bin isn't on your PATH — add it to run `vibeseek` from anywhere\x1b[0m");
    }

    write_units(&cfg, &exe)?;
    daemon::systemctl(&["daemon-reload"])?;
    daemon::systemctl(&["enable", "--now", SLSKD_UNIT])?;
    daemon::systemctl(&["enable", "--now", AGENT_UNIT])?;
    println!("started slskd {version} and the vibeseek agent (they start with your login from now on)");

    heading("Connecting");
    let client = crate::api::Client::new(&cfg)?;
    let mut logged_in = false;
    for _ in 0..60 {
        if let Ok(app) = client.application().await {
            if app.server.is_logged_in {
                println!("\x1b[32m✓ logged in to Soulseek as {}\x1b[0m", app.user.username);
                logged_in = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !logged_in {
        println!("\x1b[33mnot logged in yet. If the password was wrong (or the name is taken), fix it under soulseek: in\x1b[0m");
        println!("\x1b[33m{} and run `vibeseek daemon restart`. `vibeseek daemon logs` shows what happened.\x1b[0m", config::tilde(&cfg.slskd_yml()));
    } else {
        // Give the agent a moment to open the port, then check it.
        tokio::time::sleep(Duration::from_secs(8)).await;
        let port = crate::slskdcfg::listen_port(&cfg.slskd_yml()).unwrap_or(answers.listen_port);
        match crate::port::test(port).await {
            Ok(t) if t.open => println!("\x1b[32m✓ port {port} is reachable\x1b[0m"),
            Ok(_) => println!(
                "\x1b[33m✗ port {port} isn't reachable yet. It may take a minute (check with `vibeseek port --check`); otherwise forward TCP {port} on your router.\x1b[0m"
            ),
            Err(e) => println!("(couldn't run the port test: {e})"),
        }
    }

    heading("Done");
    println!("  vibeseek                 open the app (Search · Downloads · Uploads · History · Messages, ? for keys)");
    println!("  vibeseek search <words>  search from the command line");
    println!("  vibeseek csv <file|link> batch download a CSV or Spotify/YouTube playlist");
    println!("  vibeseek status          check everything is running");
    Ok(())
}

/// `vibeseek setup --uninstall`: stop and remove the services and slskd (keeps downloads).
pub fn uninstall(cfg: &Config) -> Result<()> {
    if cfg.slskd.service != SLSKD_UNIT {
        bail!("this slskd wasn't installed by `vibeseek setup` ({}), so uninstall won't touch it", cfg.slskd.service);
    }
    if !ask_yes("Remove vibeseek's services, slskd and settings? Your downloads stay", false)? {
        return Ok(());
    }
    for unit in [AGENT_UNIT, SLSKD_UNIT] {
        let _ = daemon::systemctl(&["disable", "--now", unit]);
        let _ = std::fs::remove_file(unit_dir().join(unit));
    }
    let _ = daemon::systemctl(&["daemon-reload"]);
    let _ = std::fs::remove_dir_all(slskd_dir());
    let _ = std::fs::remove_file(cfg.slskd_yml());
    let _ = std::fs::remove_file(config::config_file());
    let _ = std::fs::remove_file(installed_exe());
    println!("removed. Downloads, upload history ({}) and slskd's data folder were left in place.", config::tilde(&config::data_dir()));
    Ok(())
}
