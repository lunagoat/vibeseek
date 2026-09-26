//! Non-interactive subcommands.

use anyhow::{bail, Context, Result};
use comfy_table::{presets, Attribute, Cell, CellAlignment, Color, ContentArrangement, Table};
use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;
use uuid::Uuid;

use crate::api::{self, Client, Transfer};
use crate::config::{self, Config};
use crate::download::{self, Target};
use crate::history::History;
use crate::quality::{self, Filter, Folder, Hit};
use crate::search::{self, CachedItem, LastSearch};
use crate::{fmt, slskdcfg, Cli, Cmd, FilterArgs, TransferAction};

pub async fn run(cli: Cli) -> Result<()> {
    let cfg = Config::load()?;
    match cli.cmd.unwrap_or(Cmd::Tui) {
        Cmd::Tui => crate::tui::run(cfg).await,
        Cmd::Search { query, filter, albums, limit, timeout, json, download, output } => {
            let mut cfg = cfg;
            if let Some(t) = timeout {
                cfg.search.timeout_secs = t;
            }
            cmd_search(&cfg, &query.join(" "), &filter, albums, limit, json).await?;
            if let Some(sel) = download {
                cmd_get(&cfg, &[sel], output.as_deref(), false).await?;
            }
            Ok(())
        }
        Cmd::Get { selection, output, wait } => cmd_get(&cfg, &selection, output.as_deref(), wait).await,
        Cmd::Downloads { action, watch, all } => cmd_transfers(&cfg, false, action, watch, all).await,
        Cmd::Uploads { action, watch, all } => cmd_transfers(&cfg, true, action, watch, all).await,
        Cmd::History { user, since, limit } => cmd_history(&cfg, user.as_deref(), since.as_deref(), limit).await,
        Cmd::Ban { user } => cmd_ban(&cfg, &user).await,
        Cmd::Unban { user } => {
            if slskdcfg::unban(&cfg.slskd_yml(), &user)? {
                println!("unbanned {user}");
            } else {
                println!("{user} wasn't banned");
            }
            Ok(())
        }
        Cmd::Bans => {
            let bans = slskdcfg::banned(&cfg.slskd_yml())?;
            if bans.is_empty() {
                println!("no banned users");
            }
            for b in bans {
                println!("{b}");
            }
            Ok(())
        }
        Cmd::Csv(args) => crate::csvjob::run(&cfg, args).await,
        Cmd::Port { port, show } => cmd_port(&cfg, port, show),
        Cmd::Daemon { action } => crate::daemon::run(&cfg, action),
        Cmd::Agent { action } => crate::agent::run(&cfg, action).await,
        Cmd::Status => cmd_status(&cfg).await,
        Cmd::Config { edit } => {
            let path = config::config_file();
            if edit {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
                std::process::Command::new(editor).arg(&path).status()?;
            } else {
                println!("{}", path.display());
                println!("slskd config: {}", cfg.slskd_yml().display());
            }
            Ok(())
        }
    }
}

/// Build the effective filter from a preset + flag overrides.
pub fn build_filter(cfg: &Config, args: &FilterArgs, default_preset: &str) -> Result<(String, Filter)> {
    let name = args.preset.clone().unwrap_or_else(|| default_preset.to_string());
    let mut f = cfg.preset(&name)?;
    let mut label = name;
    if let Some(fmts) = &args.format {
        f.formats = fmts.iter().map(|s| s.trim().trim_start_matches('.').to_lowercase()).filter(|s| !s.is_empty()).collect();
        label = "custom".into();
    }
    if args.min_bitrate.is_some() {
        f.min_bitrate = args.min_bitrate;
        label = "custom".into();
    }
    if args.min_bitdepth.is_some() {
        f.min_bitdepth = args.min_bitdepth;
        label = "custom".into();
    }
    if args.min_samplerate.is_some() {
        f.min_samplerate = args.min_samplerate;
        label = "custom".into();
    }
    if args.strict {
        f.strict = true;
    }
    Ok((label, f))
}

fn table() -> Table {
    let mut t = Table::new();
    t.load_style(presets::NOTHING).set_content_arrangement(ContentArrangement::Dynamic);
    if let Some((w, _)) = crossterm::terminal::size().ok() {
        t.set_width(w);
    }
    t
}

fn header(t: &mut Table, cols: &[&str]) {
    t.set_header(cols.iter().map(|c| Cell::new(c).add_attribute(Attribute::Bold).fg(Color::DarkGrey)));
}

fn term_width() -> usize {
    crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(120)
}

fn peer_cell(h_free: bool, queue: u64, user: &str) -> Cell {
    if h_free {
        Cell::new(user).fg(Color::Green)
    } else if queue > 0 {
        Cell::new(format!("{user} (q{queue})")).fg(Color::Yellow)
    } else {
        Cell::new(user)
    }
}

async fn cmd_search(cfg: &Config, query: &str, fargs: &FilterArgs, albums: bool, limit: usize, json: bool) -> Result<()> {
    let client = Client::new(cfg)?;
    let (label, filter) = build_filter(cfg, fargs, &cfg.search.default_preset)?;
    let tty = !json && std::io::IsTerminal::is_terminal(&std::io::stderr());
    let out = search::run(&client, cfg, query, |r, f| {
        if tty {
            eprint!("\r\x1b[2K🔎 {query}: {r} peers, {f} files…");
            let _ = std::io::stderr().flush();
        }
    })
    .await?;
    if tty {
        eprint!("\r\x1b[2K");
    }
    let (hits, hidden) = quality::rank_query(&out.responses, &filter, &cfg.prefs, query);

    let mut last = LastSearch { query: query.to_string(), folders: albums, items: vec![] };
    if albums {
        let folders = quality::group_folders(&hits);
        last.items = folders
            .iter()
            .map(|f| CachedItem { username: f.username.clone(), path: f.dir.clone(), files: f.files.iter().map(|h| h.file.clone()).collect() })
            .collect();
        if json {
            println!("{}", serde_json::to_string_pretty(&last.items)?);
        } else {
            print_folders(&folders[..folders.len().min(limit)]);
        }
    } else {
        last.items = hits.iter().map(CachedItem::from_hit).collect();
        if json {
            println!("{}", serde_json::to_string_pretty(&hits)?);
        } else {
            print_hits(&hits[..hits.len().min(limit)]);
        }
    }
    search::save_last(&last)?;

    if !json {
        let shown = last.items.len().min(limit);
        let kind = if albums { "folders" } else { "files" };
        println!();
        println!(
            "\x1b[2m{} {kind} ({shown} shown) from {} peers · filter: {label} ({})\x1b[0m",
            last.items.len(),
            out.responses.len(),
            filter.describe()
        );
        if hidden > 0 && !filter.is_empty() {
            println!("\x1b[2m{hidden} files hidden by the filter — use -p any to see everything\x1b[0m");
        }
        if !last.items.is_empty() {
            println!("\x1b[2mdownload with: vibeseek get <numbers> [-o folder]   e.g. vibeseek get 1 3-5\x1b[0m");
        }
    }
    Ok(())
}

fn print_hits(hits: &[Hit]) {
    let mut t = table();
    header(&mut t, &["#", "quality", "size", "len", "peer", "speed", "file"]);
    let path_w = term_width().saturating_sub(70).max(30);
    for (i, h) in hits.iter().enumerate() {
        t.add_row(vec![
            Cell::new(i + 1).fg(Color::Cyan).set_alignment(CellAlignment::Right),
            Cell::new(quality::quality_label(&h.file)).fg(Color::Magenta),
            Cell::new(fmt::size(h.file.size)).set_alignment(CellAlignment::Right),
            Cell::new(h.file.length.map(|l| fmt::duration(l as u64)).unwrap_or_default()),
            peer_cell(h.free_slot, h.queue_length, &fmt::trunc(&h.username, 18)),
            Cell::new(fmt::speed(h.upload_speed as f64)).fg(Color::DarkGrey),
            Cell::new(fmt::trunc_left(&h.file.filename.replace('\\', "/"), path_w)),
        ]);
    }
    println!("{t}");
}

fn folder_quality(f: &Folder) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for h in &f.files {
        *counts.entry(quality::quality_label(&h.file)).or_default() += 1;
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(q, _)| q).unwrap_or_default()
}

fn print_folders(folders: &[Folder]) {
    let mut t = table();
    header(&mut t, &["#", "files", "quality", "size", "peer", "speed", "folder"]);
    let path_w = term_width().saturating_sub(70).max(30);
    for (i, f) in folders.iter().enumerate() {
        let h = &f.files[0];
        t.add_row(vec![
            Cell::new(i + 1).fg(Color::Cyan).set_alignment(CellAlignment::Right),
            Cell::new(f.files.len()).set_alignment(CellAlignment::Right),
            Cell::new(folder_quality(f)).fg(Color::Magenta),
            Cell::new(fmt::size(f.size())).set_alignment(CellAlignment::Right),
            peer_cell(h.free_slot, h.queue_length, &fmt::trunc(&f.username, 18)),
            Cell::new(fmt::speed(h.upload_speed as f64)).fg(Color::DarkGrey),
            Cell::new(fmt::trunc_left(&f.dir.replace('\\', "/"), path_w)),
        ]);
    }
    println!("{t}");
}

/// Queue a set of cached items. Returns batch ids.
pub async fn queue_items(client: &Client, cfg: &Config, items: &[CachedItem], folders: bool, target: &Target) -> Result<Vec<Uuid>> {
    let mut batches = vec![];
    if folders {
        for it in items {
            let files = download::folder_contents(client, &it.username, &it.path, &it.files).await;
            let dest = target.join(api::basename(&it.path));
            let (id, fails) = download::enqueue(client, cfg, &it.username, &files, &dest).await?;
            println!(
                "queued {} files from {} → {}",
                files.len() - fails.len(),
                it.username,
                dest.describe(cfg)
            );
            for f in fails {
                println!("  \x1b[31m✗\x1b[0m {f}");
            }
            batches.push(id);
        }
    } else {
        // One batch per peer.
        let mut by_user: BTreeMap<&str, Vec<api::SearchFile>> = BTreeMap::new();
        for it in items {
            by_user.entry(&it.username).or_default().extend(it.files.iter().cloned());
        }
        for (user, files) in by_user {
            let (id, fails) = download::enqueue(client, cfg, user, &files, target).await?;
            for f in &files {
                if !fails.iter().any(|x| x.starts_with(api::basename(&f.filename))) {
                    println!("queued {} from {user}", api::basename(&f.filename));
                }
            }
            for f in fails {
                println!("  \x1b[31m✗\x1b[0m {f}");
            }
            batches.push(id);
        }
        println!("→ {}", target.describe(cfg));
    }
    Ok(batches)
}

async fn cmd_get(cfg: &Config, selection: &[String], output: Option<&str>, wait: bool) -> Result<()> {
    let last = search::load_last()?;
    let sel = search::parse_selection(selection)?;
    let mut items = vec![];
    for n in sel {
        let it = last.items.get(n - 1).with_context(|| format!("no result #{n} (last search had {})", last.items.len()))?;
        items.push(it.clone());
    }
    let client = Client::new(cfg)?;
    let target = Target::from_opt(cfg, output);
    let batches = queue_items(&client, cfg, &items, last.folders, &target).await?;
    if wait {
        wait_for(&client, &batches).await?;
    } else if download::pending_count() > 0 && !crate::daemon::is_active(crate::daemon::AGENT_UNIT) {
        println!("\x1b[33mnote:\x1b[0m files will be moved into place by the agent (`vibeseek agent install`), the TUI, or `get --wait`");
    }
    Ok(())
}

/// Show progress for the given batches until they're all finished.
pub async fn wait_for(client: &Client, batches: &[Uuid]) -> Result<()> {
    let mut printed = 0;
    loop {
        let all = client.downloads().await?;
        let mine: Vec<&Transfer> = all.iter().filter(|t| t.batch_id.map(|b| batches.contains(&b)).unwrap_or(false)).collect();
        download::process_moves(client).await.ok();
        // Redraw the block of lines in place.
        if printed > 0 {
            print!("\x1b[{printed}A");
        }
        let w = term_width().saturating_sub(50).max(20);
        for t in &mine {
            println!(
                "\x1b[2K{} {:>5.1}% {:>10} {:<16} {}",
                fmt::bar(t.percent_complete, 20),
                t.percent_complete,
                fmt::speed(t.average_speed),
                fmt::trunc(&t.short_state(), 16),
                fmt::trunc(t.basename(), w)
            );
        }
        printed = mine.len();
        if !mine.is_empty() && mine.iter().all(|t| t.is_finished()) {
            // One more pass so staged files get moved.
            tokio::time::sleep(Duration::from_millis(800)).await;
            download::process_moves(client).await.ok();
            let ok = mine.iter().filter(|t| t.is_succeeded()).count();
            println!("done: {ok}/{} succeeded", mine.len());
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn last_list_path(uploads: bool) -> std::path::PathBuf {
    config::cache_dir().join(if uploads { "last_uploads.json" } else { "last_downloads.json" })
}

fn state_cell(t: &Transfer) -> Cell {
    let c = Cell::new(t.short_state());
    if t.is_succeeded() {
        c.fg(Color::Green)
    } else if t.is_failed() {
        c.fg(Color::Red)
    } else if t.is_active() {
        c.fg(Color::Cyan)
    } else {
        c.fg(Color::Yellow)
    }
}

fn render_transfers(list: &[Transfer], uploads: bool) -> String {
    let mut t = table();
    header(&mut t, &["#", if uploads { "downloader" } else { "from" }, "state", "progress", "speed", "eta", "size", "file"]);
    let w = term_width().saturating_sub(95).max(24);
    for (i, x) in list.iter().enumerate() {
        t.add_row(vec![
            Cell::new(i + 1).fg(Color::Cyan).set_alignment(CellAlignment::Right),
            Cell::new(fmt::trunc(&x.username, 18)),
            state_cell(x),
            Cell::new(format!("{} {:>3.0}%", fmt::bar(x.percent_complete, 12), x.percent_complete)),
            Cell::new(if x.is_active() { fmt::speed(x.average_speed) } else { "-".into() }),
            Cell::new(if x.is_active() { fmt::eta(x.size.saturating_sub(x.bytes_transferred), x.average_speed) } else { "".into() }),
            Cell::new(fmt::size(x.size)).set_alignment(CellAlignment::Right),
            Cell::new(fmt::trunc(x.basename(), w)),
        ]);
    }
    t.to_string()
}

fn summary(list: &[Transfer], uploads: bool) -> String {
    let active: Vec<_> = list.iter().filter(|t| t.is_active()).collect();
    let queued = list.iter().filter(|t| t.is_queued()).count();
    let speed: f64 = active.iter().map(|t| t.average_speed).sum();
    let users: std::collections::HashSet<_> = list.iter().filter(|t| !t.is_finished()).map(|t| &t.username).collect();
    format!(
        "{} active · {} queued · {} · {} {}",
        active.len(),
        queued,
        fmt::speed(speed),
        users.len(),
        if uploads { "people downloading from you" } else { "peers" }
    )
}

async fn fetch(client: &Client, uploads: bool, all: bool) -> Result<Vec<Transfer>> {
    let mut list = if uploads { client.uploads().await? } else { client.downloads().await? };
    if !all {
        // Keep active/queued, plus things that finished in the last 10 minutes.
        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
        list.retain(|t| !t.is_finished() || t.ended_at.map(|e| e > cutoff).unwrap_or(false));
    }
    // Active first, then queued, then finished.
    list.sort_by_key(|t| (if t.is_active() { 0 } else if !t.is_finished() { 1 } else { 2 }, std::cmp::Reverse(t.requested_at)));
    Ok(list)
}

async fn cmd_transfers(cfg: &Config, uploads: bool, action: Option<TransferAction>, watch: bool, all: bool) -> Result<()> {
    let client = Client::new(cfg)?;
    if let Some(action) = action {
        return transfer_action(cfg, &client, uploads, action).await;
    }
    let history = if uploads { History::open().ok() } else { None };
    if !watch {
        let list = fetch(&client, uploads, all).await?;
        if let Some(h) = &history {
            let _ = h.record(&list);
        }
        std::fs::write(last_list_path(uploads), serde_json::to_vec(&list.iter().map(|t| (t.username.clone(), t.id)).collect::<Vec<_>>())?)?;
        if list.is_empty() {
            println!("{}", if uploads { "nobody is downloading from you right now" } else { "no downloads" });
            if !all {
                println!("\x1b[2m(use --all to include older finished transfers)\x1b[0m");
            }
            return Ok(());
        }
        println!("{}", render_transfers(&list, uploads));
        println!("\n\x1b[2m{}\x1b[0m", summary(&list, uploads));
        return Ok(());
    }
    // Watch mode: redraw every second until Ctrl-C.
    let title = if uploads { "Uploads — people downloading from you" } else { "Downloads" };
    print!("\x1b[?1049h\x1b[?25l");
    let result: Result<()> = async {
        loop {
            let list = fetch(&client, uploads, all).await?;
            if let Some(h) = &history {
                let _ = h.record(&list);
            }
            if !uploads {
                download::process_moves(&client).await.ok();
            }
            let body = if list.is_empty() { "  (nothing right now)".to_string() } else { render_transfers(&list, uploads) };
            print!(
                "\x1b[H\x1b[2J\x1b[1m{title}\x1b[0m   \x1b[2m{} · {} · Ctrl-C to exit\x1b[0m\n\n{body}\n",
                summary(&list, uploads),
                chrono::Local::now().format("%H:%M:%S")
            );
            std::io::stdout().flush()?;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        Ok(())
    }
    .await;
    print!("\x1b[?25h\x1b[?1049l");
    std::io::stdout().flush()?;
    result
}

async fn transfer_action(cfg: &Config, client: &Client, uploads: bool, action: TransferAction) -> Result<()> {
    let current = if uploads { client.uploads().await? } else { client.downloads().await? };
    match action {
        TransferAction::Clear => {
            if uploads {
                client.clear_completed_uploads().await?;
                let _ = History::open().and_then(|h| h.record(&current));
            } else {
                client.clear_completed_downloads().await?;
            }
            println!("cleared finished transfers");
        }
        TransferAction::Cancel { which } => {
            let targets = select_transfers(&current, uploads, &which, |t| !t.is_finished())?;
            for t in &targets {
                if uploads {
                    client.cancel_upload(t, false).await?;
                } else {
                    client.cancel_download(t, false).await?;
                }
                println!("cancelled {} ({})", t.basename(), t.username);
            }
            if targets.is_empty() {
                println!("nothing to cancel");
            }
        }
        TransferAction::Retry { which } => {
            if uploads {
                bail!("retry only applies to downloads");
            }
            let targets = if which.is_empty() {
                current.iter().filter(|t| t.is_failed()).cloned().collect()
            } else {
                select_transfers(&current, false, &which, |t| t.is_failed())?
            };
            let mut by_user: BTreeMap<String, Vec<api::SearchFile>> = BTreeMap::new();
            for t in &targets {
                client.cancel_download(t, true).await.ok();
                by_user.entry(t.username.clone()).or_default().push(api::SearchFile { filename: t.filename.clone(), size: t.size, ..Default::default() });
            }
            for (user, files) in by_user {
                download::enqueue(client, cfg, &user, &files, &Target::Default).await?;
                println!("re-queued {} file(s) from {user}", files.len());
            }
            if targets.is_empty() {
                println!("no failed downloads");
            }
        }
    }
    Ok(())
}

/// Resolve "3", "1-4", "all", "user:NAME" against the last listing.
fn select_transfers(current: &[Transfer], uploads: bool, which: &[String], all_pred: impl Fn(&Transfer) -> bool) -> Result<Vec<Transfer>> {
    let mut out = vec![];
    let mut nums = vec![];
    for w in which {
        if w == "all" {
            out.extend(current.iter().filter(|t| all_pred(t)).cloned());
        } else if let Some(user) = w.strip_prefix("user:") {
            out.extend(current.iter().filter(|t| t.username == user && all_pred(t)).cloned());
        } else {
            nums.push(w.clone());
        }
    }
    if !nums.is_empty() {
        let data = std::fs::read(last_list_path(uploads)).context("list transfers first so they have numbers")?;
        let ids: Vec<(String, Uuid)> = serde_json::from_slice(&data)?;
        for n in search::parse_selection(&nums)? {
            let (_, id) = ids.get(n - 1).with_context(|| format!("no transfer #{n} in the last listing"))?;
            let t = current.iter().find(|t| t.id == *id).with_context(|| format!("transfer #{n} is gone"))?;
            out.push(t.clone());
        }
    }
    Ok(out)
}

pub async fn ban_user(cfg: &Config, client: &Client, user: &str) -> Result<usize> {
    slskdcfg::ban(&cfg.slskd_yml(), user)?;
    let mut n = 0;
    for t in client.uploads().await?.iter().filter(|t| t.username == user && !t.is_finished()) {
        client.cancel_upload(t, true).await.ok();
        n += 1;
    }
    Ok(n)
}

async fn cmd_ban(cfg: &Config, user: &str) -> Result<()> {
    let client = Client::new(cfg)?;
    let n = ban_user(cfg, &client, user).await?;
    println!("banned {user}{}", if n > 0 { format!(" and cancelled {n} upload(s)") } else { String::new() });
    Ok(())
}

pub fn parse_since(s: &str) -> Result<String> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().context("use e.g. 24h, 7d, 4w")?;
    let d = match unit {
        "h" => chrono::Duration::hours(n),
        "d" | "" => chrono::Duration::days(n),
        "w" => chrono::Duration::weeks(n),
        "m" => chrono::Duration::days(n * 30),
        _ => bail!("unknown unit '{unit}' (h, d, w, m)"),
    };
    Ok((chrono::Utc::now() - d).to_rfc3339())
}

async fn cmd_history(cfg: &Config, user: Option<&str>, since: Option<&str>, limit: usize) -> Result<()> {
    let h = History::open()?;
    // Pull in anything slskd still remembers.
    if let Ok(client) = Client::new(cfg) {
        if let Ok(ups) = client.uploads().await {
            let _ = h.record(&ups);
        }
    }
    let since_ts = since.map(parse_since).transpose()?;
    let since_ref = since_ts.as_deref();
    let window = since.map(|s| format!("last {s}")).unwrap_or_else(|| "all time".into());

    if let Some(u) = user {
        let rows = h.recent(limit, Some(u))?;
        println!("\x1b[1mUploads to {u}\x1b[0m");
        print_history_rows(&rows);
        return Ok(());
    }
    let tot = h.totals(since_ref)?;
    println!(
        "\x1b[1mUpload stats ({window})\x1b[0m  {} files · {} served · {} people · {} failed/cancelled\n",
        tot.files,
        fmt::size(tot.bytes),
        tot.users,
        tot.failed
    );
    let mut t = table();
    header(&mut t, &["top downloaders", "files", "data"]);
    for (u, n, b) in h.top_users(limit, since_ref)? {
        t.add_row(vec![Cell::new(u), Cell::new(n).set_alignment(CellAlignment::Right), Cell::new(fmt::size(b)).set_alignment(CellAlignment::Right)]);
    }
    println!("{t}\n");
    let mut t = table();
    header(&mut t, &["most downloaded", "times", "people"]);
    let w = term_width().saturating_sub(20).max(30);
    for (f, n, u) in h.top_files(limit, since_ref)? {
        t.add_row(vec![
            Cell::new(fmt::trunc_left(&f.replace('\\', "/"), w)),
            Cell::new(n).set_alignment(CellAlignment::Right),
            Cell::new(u).set_alignment(CellAlignment::Right),
        ]);
    }
    println!("{t}\n");
    println!("\x1b[1mRecent\x1b[0m");
    print_history_rows(&h.recent(limit, None)?);
    if tot.files == 0 {
        println!("\n\x1b[2mhistory is recorded while `vibeseek agent` (or the TUI) runs — `vibeseek agent install`\x1b[0m");
    }
    Ok(())
}

fn print_history_rows(rows: &[crate::history::Row]) {
    let mut t = table();
    header(&mut t, &["when", "user", "state", "size", "file"]);
    let w = term_width().saturating_sub(60).max(24);
    for r in rows {
        let when = chrono::DateTime::parse_from_rfc3339(&r.ended_at)
            .map(|d| d.with_timezone(&chrono::Local).format("%b %d %H:%M").to_string())
            .unwrap_or_default();
        let ok = r.state.ends_with("Succeeded");
        t.add_row(vec![
            Cell::new(when).fg(Color::DarkGrey),
            Cell::new(&r.username),
            Cell::new(r.state.trim_start_matches("Completed, ")).fg(if ok { Color::Green } else { Color::Red }),
            Cell::new(fmt::size(r.size)).set_alignment(CellAlignment::Right),
            Cell::new(fmt::trunc(api::basename(&r.filename), w)),
        ]);
    }
    println!("{t}");
}

fn cmd_port(cfg: &Config, port: Option<u16>, show: bool) -> Result<()> {
    let yml = cfg.slskd_yml();
    let current = slskdcfg::listen_port(&yml)?;
    if show {
        println!("slskd listen port: {current}");
        match crate::port::detect(&cfg.port.gateway) {
            Ok(p) => println!("VPN forwarded port: {p}{}", if p == current { " ✓" } else { "  (mismatch — run `vibeseek port`)" }),
            Err(e) => println!("VPN forwarded port: unavailable ({e})"),
        }
        return Ok(());
    }
    let p = match port {
        Some(p) => p,
        None => crate::port::detect(&cfg.port.gateway)?,
    };
    match crate::port::apply(cfg, p)? {
        crate::port::SyncResult::Unchanged(p) => println!("port already {p}"),
        crate::port::SyncResult::Changed { from, to } => println!("listen port {from} → {to} (slskd applies it live)"),
    }
    Ok(())
}

async fn cmd_status(cfg: &Config) -> Result<()> {
    let running = crate::daemon::is_active(&cfg.slskd.service);
    let agent = crate::daemon::is_active(crate::daemon::AGENT_UNIT);
    println!("slskd service:  {}", if running { "\x1b[32mrunning\x1b[0m" } else { "\x1b[31mstopped\x1b[0m" });
    println!("vibeseek agent: {}", if agent { "\x1b[32mrunning\x1b[0m" } else { "\x1b[33mnot running\x1b[0m (vibeseek agent install)" });
    if let Ok(p) = slskdcfg::listen_port(&cfg.slskd_yml()) {
        println!("listen port:    {p}");
    }
    let client = Client::new(cfg)?;
    match client.application().await {
        Ok(app) => {
            let st = &app.server;
            let conn = if st.is_logged_in { format!("\x1b[32m{}\x1b[0m", st.state) } else { format!("\x1b[33m{}\x1b[0m", st.state) };
            println!("soulseek:       {conn} as {}", app.user.username);
            let sh = &app.shares;
            println!(
                "shares:         {} files in {} folders{}",
                sh.files,
                sh.directories,
                if sh.scanning { format!(" (scanning {:.0}%)", sh.scan_progress * 100.0) } else { String::new() }
            );
            println!("downloads dir:  {}", config::tilde(&cfg.downloads_dir()));
            if app.pending_restart {
                println!("\x1b[33mslskd needs a restart to apply config changes (vibeseek daemon restart)\x1b[0m");
            }
        }
        Err(e) => println!("soulseek:       \x1b[31m{e}\x1b[0m"),
    }
    Ok(())
}
