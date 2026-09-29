//! Full-screen terminal interface: state, background workers, and key handling.
//! Rendering lives in `ui.rs`.

mod ui;

use anyhow::Result;
use chrono::{Local, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::api::{self, AppState, Client, Conversation, PrivateMessage, SearchFile, SearchResponse, Transfer};
use crate::config::Config;
use crate::download::{self, Target};
use crate::history::{self, History};
use crate::quality::{self, Filter, Folder, Hit};
use crate::search;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Downloads,
    Uploads,
    History,
    Messages,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Search, Tab::Downloads, Tab::Uploads, Tab::History, Tab::Messages];
    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap()
    }
    fn cycle(self, d: isize) -> Tab {
        Self::ALL[(self.index() as isize + d).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

/// What keystrokes currently go to.
pub enum Mode {
    Normal,
    SearchInput,
    /// Output folder prompt (its own buffer so the search text survives).
    OutputPrompt(String),
    ConfirmBan(String),
    UserDetail(String, Vec<history::Row>),
    /// Typing a private message to the open conversation.
    Compose(String),
    /// Typing the username for a new conversation.
    NewConversation(String),
    /// Typing file types to show (e.g. "mkv", "dsd, flac").
    TypesPrompt(String),
    Help,
}

enum Msg {
    App(Result<(AppState, u16), String>),
    Downloads(Result<Vec<Transfer>, String>),
    Uploads(Result<Vec<Transfer>, String>),
    SearchProgress(u32, u32),
    SearchDone(Result<Vec<SearchResponse>, String>),
    Info(String),
    Error(String),
    History(Box<HistData>),
    UserDetail(String, Vec<history::Row>),
    Conversations(Vec<ConvRow>),
    Thread(String, Vec<PrivateMessage>),
    PortTest(bool, String),
}

/// A conversation plus its latest message (for sorting and previews).
pub struct ConvRow {
    pub conv: Conversation,
    pub last: Option<PrivateMessage>,
}

#[derive(Default)]
pub struct HistData {
    pub all: history::Totals,
    pub week: history::Totals,
    pub today: history::Totals,
    pub users: Vec<(String, u64, u64)>,
    pub files: Vec<(String, u64, u64)>,
    pub recent: Vec<history::Row>,
    pub bans: Vec<String>,
}

pub struct App {
    pub cfg: Config,
    client: Client,
    tx: mpsc::UnboundedSender<Msg>,
    pub tab: Tab,
    pub mode: Mode,
    // connection
    pub app: Option<AppState>,
    pub port: Option<u16>,
    pub offline: Option<String>,
    // transfers (raw + the filtered/sorted rows the tables show)
    downloads: Vec<Transfer>,
    uploads: Vec<Transfer>,
    pub dl_view: Vec<Transfer>,
    pub ul_view: Vec<Transfer>,
    pub hide_old: bool,
    // search
    responses: Vec<SearchResponse>,
    pub hits: Vec<Hit>,
    pub folders: Vec<Folder>,
    pub hidden: usize,
    pub query: String,
    pub input: String,
    pub folders_view: bool,
    pub preset: usize,
    pub preset_names: Vec<String>,
    /// File types chosen with `t`, overriding the preset's.
    pub types: Option<Vec<String>>,
    pub target: Target,
    pub sort: usize,
    pub searching: Option<Instant>,
    pub progress: (u32, u32),
    // history
    pub hist: HistData,
    last_hist: Instant,
    // messages
    pub convs: Vec<ConvRow>,
    /// Conversation shown on the right (also what the background worker keeps fresh).
    pub thread_user: Option<String>,
    pub thread: Vec<PrivateMessage>,
    open_conv: tokio::sync::watch::Sender<Option<String>>,
    // ui
    pub status: Option<(String, Instant, bool)>,
    pub tables: [TableState; 5],
    pub port_test: Option<(bool, String)>,
}

pub const SORTS: [&str; 4] = ["best", "size", "speed", "peer"];

pub async fn run(cfg: Config) -> Result<()> {
    let client = Client::new(&cfg)?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let preset_names: Vec<String> = cfg.presets.keys().cloned().collect();
    let (open_conv, open_rx) = tokio::sync::watch::channel(None);
    let mut app = App {
        preset: preset_names.iter().position(|k| *k == cfg.search.default_preset).unwrap_or(0),
        preset_names,
        types: None,
        target: Target::from_opt(&cfg, None),
        cfg,
        client,
        tx,
        tab: Tab::Search,
        mode: Mode::SearchInput,
        app: None,
        port: None,
        offline: None,
        downloads: vec![],
        uploads: vec![],
        dl_view: vec![],
        ul_view: vec![],
        // Show finished transfers by default; `h` hides ones older than 10 minutes.
        hide_old: false,
        responses: vec![],
        hits: vec![],
        folders: vec![],
        hidden: 0,
        query: String::new(),
        input: String::new(),
        folders_view: false,
        sort: 0,
        searching: None,
        progress: (0, 0),
        hist: HistData::default(),
        last_hist: Instant::now() - Duration::from_secs(60),
        convs: vec![],
        thread_user: None,
        thread: vec![],
        open_conv,
        status: None,
        tables: Default::default(),
        port_test: None,
    };
    spawn_workers(&app, open_rx);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut rx).await;
    ratatui::restore();
    result
}

fn spawn_workers(a: &App, mut open_rx: tokio::sync::watch::Receiver<Option<String>>) {
    // Conversations every few seconds; the open one is fetched (and marked read) right away
    // whenever it changes.
    let (c, tx) = (a.client.clone(), a.tx.clone());
    tokio::spawn(async move {
        loop {
            if let Ok(convs) = c.conversations().await {
                let mut rows = vec![];
                for conv in convs {
                    let last = c.messages(&conv.username).await.ok().and_then(|m| m.last().cloned());
                    rows.push(ConvRow { conv, last });
                }
                rows.sort_by_key(|r| (r.conv.un_acknowledged_message_count == 0, std::cmp::Reverse(r.last.as_ref().and_then(|m| m.timestamp))));
                if tx.send(Msg::Conversations(rows)).is_err() {
                    break;
                }
            }
            let open = open_rx.borrow_and_update().clone();
            if let Some(u) = open {
                if let Ok(msgs) = c.messages(&u).await {
                    if msgs.iter().any(|m| m.is_incoming()) {
                        let _ = c.ack_conversation(&u).await;
                    }
                    let _ = tx.send(Msg::Thread(u, msgs));
                }
            }
            // Wake early when a different conversation is opened.
            let _ = tokio::time::timeout(Duration::from_secs(4), open_rx.changed()).await;
        }
    });

    // Transfers every second; record finished uploads into history as we see them.
    let (c, tx) = (a.client.clone(), a.tx.clone());
    tokio::spawn(async move {
        loop {
            let d = c.downloads().await.map_err(|e| e.to_string());
            let u = c.uploads().await.map_err(|e| e.to_string());
            if let Ok(list) = &u {
                let list = list.clone();
                let _ = tokio::task::spawn_blocking(move || History::open().and_then(|h| h.record(&list))).await;
            }
            if tx.send(Msg::Downloads(d)).is_err() || tx.send(Msg::Uploads(u)).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    // Connection state + port every 5s.
    let (c, tx, yml) = (a.client.clone(), a.tx.clone(), a.cfg.slskd_yml());
    tokio::spawn(async move {
        loop {
            let r = c.application().await.map_err(|e| e.to_string());
            let port = crate::slskdcfg::listen_port(&yml).unwrap_or(0);
            if tx.send(Msg::App(r.map(|s| (s, port)))).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    // Move finished staged downloads into their folders.
    let c = a.client.clone();
    tokio::spawn(async move {
        loop {
            let _ = download::process_moves(&c).await;
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    });
}

async fn event_loop(term: &mut ratatui::DefaultTerminal, a: &mut App, rx: &mut mpsc::UnboundedReceiver<Msg>) -> Result<()> {
    loop {
        while let Ok(m) = rx.try_recv() {
            a.on_msg(m);
        }
        if a.last_hist.elapsed() > Duration::from_secs(10) {
            a.refresh_history();
        }
        term.draw(|f| ui::render(f, a))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && a.on_key(k) {
                    return Ok(());
                }
            }
        }
    }
}

fn is_old(t: &Transfer) -> bool {
    t.is_finished() && t.ended_at.map(|d| Utc::now() - d > chrono::Duration::minutes(10)).unwrap_or(true)
}

/// Active first, then queued, then finished; newest first within each group.
fn sorted_view(list: &[Transfer], hide_old: bool) -> Vec<Transfer> {
    let mut v: Vec<Transfer> = list.iter().filter(|t| !(hide_old && is_old(t))).cloned().collect();
    v.sort_by_key(|t| (if t.is_active() { 0 } else if !t.is_finished() { 1 } else { 2 }, std::cmp::Reverse(t.requested_at)));
    v
}

/// Replace a view while keeping the same transfer selected.
fn keep_selection(old: &[Transfer], new: &[Transfer], st: &mut TableState) {
    let id = st.selected().and_then(|i| old.get(i)).map(|t| t.id);
    let idx = id.and_then(|id| new.iter().position(|t| t.id == id));
    st.select(match (idx, st.selected()) {
        (Some(i), _) => Some(i),
        _ if new.is_empty() => None,
        (None, Some(i)) => Some(i.min(new.len() - 1)),
        (None, None) => Some(0),
    });
}

impl App {
    fn say(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now(), false));
    }
    fn err(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now(), true));
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::App(Ok((s, port))) => {
                self.app = Some(s);
                self.port = Some(port);
                self.offline = None;
            }
            Msg::App(Err(e)) => self.offline = Some(e),
            Msg::Downloads(Ok(v)) => {
                self.downloads = v;
                self.rebuild_views();
            }
            Msg::Uploads(Ok(v)) => {
                self.uploads = v;
                self.rebuild_views();
            }
            Msg::Downloads(Err(e)) | Msg::Uploads(Err(e)) => self.offline = Some(e),
            Msg::SearchProgress(p, f) => self.progress = (p, f),
            Msg::SearchDone(Ok(r)) => {
                self.searching = None;
                self.responses = r;
                self.rerank();
                let n = if self.folders_view { self.folders.len() } else { self.hits.len() };
                self.say(format!("{n} results from {} peers", self.responses.len()));
            }
            Msg::SearchDone(Err(e)) => {
                self.searching = None;
                self.err(e);
            }
            Msg::Info(s) => self.say(s),
            Msg::Error(s) => self.err(s),
            Msg::History(h) => {
                self.hist = *h;
                let n = self.hist.bans.len();
                let st = &mut self.tables[Tab::History.index()];
                st.select(if n == 0 { None } else { Some(st.selected().unwrap_or(0).min(n - 1)) });
            }
            Msg::Conversations(rows) => {
                // Keep the same conversation selected as the order changes.
                let sel = self.selected(Tab::Messages).and_then(|i| self.convs.get(i)).map(|r| r.conv.username.clone());
                self.convs = rows;
                let idx = sel.and_then(|u| self.convs.iter().position(|r| r.conv.username == u));
                let n = self.convs.len();
                self.tables[Tab::Messages.index()].select(match idx {
                    Some(i) => Some(i),
                    None if n > 0 => Some(0),
                    None => None,
                });
                if self.tab == Tab::Messages {
                    self.sync_open_conversation();
                }
            }
            Msg::Thread(user, msgs) => {
                if self.thread_user.as_deref() == Some(user.as_str()) {
                    self.thread = msgs;
                }
            }
            Msg::PortTest(open, message) => {
                self.port_test = Some((open, message.clone()));
                if open { self.say(format!("port open ✓ {message}")) } else { self.err(format!("port NOT reachable: {message}")) }
            }
            Msg::UserDetail(user, rows) => self.mode = Mode::UserDetail(user, rows),
        }
    }

    fn rebuild_views(&mut self) {
        let dl = sorted_view(&self.downloads, self.hide_old);
        keep_selection(&self.dl_view, &dl, &mut self.tables[1]);
        self.dl_view = dl;
        let ul = sorted_view(&self.uploads, self.hide_old);
        keep_selection(&self.ul_view, &ul, &mut self.tables[2]);
        self.ul_view = ul;
    }

    pub fn filter(&self) -> Filter {
        let mut f = self.preset_names.get(self.preset).and_then(|n| self.cfg.presets.get(n)).cloned().unwrap_or_default();
        if let Some(t) = &self.types {
            f.formats = t.clone();
        }
        f
    }

    fn rerank(&mut self) {
        let (mut hits, hidden) = quality::rank_query(&self.responses, &self.filter(), &self.cfg.prefs, &self.query);
        match self.sort {
            1 => hits.sort_by_key(|h| std::cmp::Reverse(h.file.size)),
            2 => hits.sort_by_key(|h| std::cmp::Reverse(h.upload_speed)),
            3 => hits.sort_by(|x, y| x.username.to_lowercase().cmp(&y.username.to_lowercase())),
            _ => {}
        }
        self.folders = quality::group_folders(&hits);
        match self.sort {
            1 => self.folders.sort_by_key(|f| std::cmp::Reverse(f.size())),
            2 => self.folders.sort_by_key(|f| std::cmp::Reverse(f.files[0].upload_speed)),
            3 => self.folders.sort_by(|x, y| x.username.to_lowercase().cmp(&y.username.to_lowercase())),
            _ => {}
        }
        self.hits = hits;
        self.hidden = hidden;
        let n = self.list_len(Tab::Search);
        self.tables[0].select(if n > 0 { Some(0) } else { None });
    }

    fn refresh_history(&mut self) {
        self.last_hist = Instant::now();
        let tx = self.tx.clone();
        let yml = self.cfg.slskd_yml();
        tokio::task::spawn_blocking(move || {
            let week = (Utc::now() - chrono::Duration::days(7)).to_rfc3339();
            let midnight = Local::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|d| d.and_local_timezone(Local).earliest())
                .map(|d| d.with_timezone(&Utc).to_rfc3339())
                .unwrap_or_default();
            let r = History::open().and_then(|h| {
                Ok(HistData {
                    all: h.totals(None)?,
                    week: h.totals(Some(&week))?,
                    today: h.totals(Some(&midnight))?,
                    users: h.top_users(12, None)?,
                    files: h.top_files(12, None)?,
                    recent: h.recent(40, None)?,
                    bans: crate::slskdcfg::banned(&yml).unwrap_or_default(),
                })
            });
            let _ = tx.send(match r {
                Ok(h) => Msg::History(Box::new(h)),
                Err(e) => Msg::Error(format!("history: {e}")),
            });
        });
    }

    pub fn list_len(&self, tab: Tab) -> usize {
        match tab {
            Tab::Search if self.folders_view => self.folders.len(),
            Tab::Search => self.hits.len(),
            Tab::Downloads => self.dl_view.len(),
            Tab::Uploads => self.ul_view.len(),
            Tab::History => self.hist.bans.len(),
            Tab::Messages => self.convs.len(),
        }
    }

    fn move_sel(&mut self, d: isize) {
        let n = self.list_len(self.tab);
        let st = &mut self.tables[self.tab.index()];
        if n == 0 {
            st.select(None);
            return;
        }
        let cur = st.selected().unwrap_or(0) as isize;
        st.select(Some((cur + d).clamp(0, n as isize - 1) as usize));
    }

    fn selected(&self, tab: Tab) -> Option<usize> {
        self.tables[tab.index()].selected()
    }

    fn selected_transfer(&self) -> Option<Transfer> {
        let i = self.selected(self.tab)?;
        match self.tab {
            Tab::Downloads => self.dl_view.get(i).cloned(),
            Tab::Uploads => self.ul_view.get(i).cloned(),
            _ => None,
        }
    }

    // ---------- actions ----------

    fn start_search(&mut self) {
        if self.searching.is_some() || self.query.is_empty() {
            return;
        }
        self.searching = Some(Instant::now());
        self.progress = (0, 0);
        let (c, cfg, tx, q) = (self.client.clone(), self.cfg.clone(), self.tx.clone(), self.query.clone());
        tokio::spawn(async move {
            let ptx = tx.clone();
            let r = search::run(&c, &cfg, &q, move |p, f| {
                let _ = ptx.send(Msg::SearchProgress(p, f));
            })
            .await
            .map(|o| o.responses)
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::SearchDone(r));
        });
    }

    /// Queue files (or a whole remote folder) in the background.
    fn queue(&mut self, username: String, dir: Option<String>, files: Vec<SearchFile>) {
        let (c, cfg, tx, target) = (self.client.clone(), self.cfg.clone(), self.tx.clone(), self.target.clone());
        self.say(format!("queueing from {username}…"));
        tokio::spawn(async move {
            let (files, dest) = match &dir {
                Some(d) => (download::folder_contents(&c, &username, d, &files).await, target.join(api::basename(d))),
                None => (files, target),
            };
            let msg = match download::enqueue(&c, &cfg, &username, &files, &dest).await {
                Ok((_, fails)) if fails.len() == files.len() => Msg::Error(format!("{username} refused: {}", fails.join("; "))),
                Ok((_, fails)) => {
                    let what = if files.len() == 1 { api::basename(&files[0].filename).to_string() } else { format!("{} files", files.len() - fails.len()) };
                    Msg::Info(format!("queued {what} from {username} → {}", dest.describe(&cfg)))
                }
                Err(e) => Msg::Error(e.to_string()),
            };
            let _ = tx.send(msg);
        });
    }

    fn download_selected(&mut self, whole_folder: bool) {
        let Some(i) = self.selected(Tab::Search) else { return };
        if self.folders_view {
            if let Some(f) = self.folders.get(i).cloned() {
                self.queue(f.username.clone(), Some(f.dir.clone()), f.files.iter().map(|h| h.file.clone()).collect());
            }
        } else if let Some(h) = self.hits.get(i).cloned() {
            let dir = whole_folder.then(|| h.dir().to_string());
            self.queue(h.username.clone(), dir, vec![h.file.clone()]);
        }
    }

    fn spawn_simple(&self, fut: impl std::future::Future<Output = Result<String>> + Send + 'static) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(match fut.await {
                Ok(s) => Msg::Info(s),
                Err(e) => Msg::Error(e.to_string()),
            });
        });
    }

    fn cancel_selected(&mut self) {
        let Some(t) = self.selected_transfer() else { return };
        if t.is_finished() {
            self.say("already finished (x clears finished transfers)");
            return;
        }
        let c = self.client.clone();
        let up = self.tab == Tab::Uploads;
        self.spawn_simple(async move {
            if up { c.cancel_upload(&t, false).await? } else { c.cancel_download(&t, false).await? }
            Ok(format!("cancelled {}", t.basename()))
        });
    }

    fn clear_finished(&mut self) {
        if self.tab == Tab::Uploads {
            self.say("uploads are kept as history — press h to hide old finished ones");
            return;
        }
        let c = self.client.clone();
        self.spawn_simple(async move {
            c.clear_completed_downloads().await?;
            Ok("cleared finished downloads".into())
        });
    }

    fn retry_selected(&mut self) {
        let Some(t) = self.selected_transfer() else { return };
        if !t.is_failed() {
            self.say("only failed downloads can be retried");
            return;
        }
        let (c, cfg) = (self.client.clone(), self.cfg.clone());
        self.spawn_simple(async move {
            c.cancel_download(&t, true).await.ok();
            let f = SearchFile { filename: t.filename.clone(), size: t.size, ..Default::default() };
            download::enqueue(&c, &cfg, &t.username, &[f], &Target::Default).await?;
            Ok(format!("re-queued {}", t.basename()))
        });
    }

    fn open_user_detail(&mut self) {
        let Some(t) = self.selected_transfer() else { return };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let rows = History::open().and_then(|h| h.recent(25, Some(&t.username))).unwrap_or_default();
            let _ = tx.send(Msg::UserDetail(t.username.clone(), rows));
        });
    }

    fn ban(&mut self, user: String) {
        let (c, cfg) = (self.client.clone(), self.cfg.clone());
        self.spawn_simple(async move {
            let n = crate::cli::ban_user(&cfg, &c, &user).await?;
            Ok(format!("banned {user}{}", if n > 0 { format!(", cancelled {n} upload(s)") } else { String::new() }))
        });
        self.last_hist = Instant::now() - Duration::from_secs(60); // refresh ban list soon
    }

    fn unban_selected(&mut self) {
        let Some(user) = self.selected(Tab::History).and_then(|i| self.hist.bans.get(i)).cloned() else {
            self.say("no banned users");
            return;
        };
        match crate::slskdcfg::unban(&self.cfg.slskd_yml(), &user) {
            Ok(_) => self.say(format!("unbanned {user}")),
            Err(e) => self.err(e.to_string()),
        }
        self.refresh_history();
    }

    /// Show the selected conversation (the worker fetches it and marks it read).
    fn sync_open_conversation(&mut self) {
        let user = self.selected(Tab::Messages).and_then(|i| self.convs.get(i)).map(|r| r.conv.username.clone());
        // A brand-new conversation (from `n`) stays open until it shows up in the list.
        let user = match (&self.thread_user, user) {
            (Some(open), _) if !self.convs.iter().any(|r| &r.conv.username == open) => Some(open.clone()),
            (_, u) => u,
        };
        if user != self.thread_user {
            self.thread.clear();
            self.thread_user = user.clone();
        }
        self.open_conv.send_if_modified(|cur| {
            if *cur != user {
                *cur = user;
                true
            } else {
                false
            }
        });
    }

    fn send_message(&mut self, text: String) {
        let Some(user) = self.thread_user.clone() else { return };
        let c = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match c.send_message(&user, &text).await {
                Ok(()) => {
                    if let Ok(msgs) = c.messages(&user).await {
                        let _ = tx.send(Msg::Thread(user.clone(), msgs));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("couldn't send to {user}: {e}")));
                }
            }
        });
    }

    fn close_selected_conversation(&mut self) {
        let Some(user) = self.thread_user.clone() else { return };
        let c = self.client.clone();
        self.spawn_simple(async move {
            c.close_conversation(&user).await?;
            Ok(format!("closed conversation with {user}"))
        });
        self.thread_user = None;
        self.thread.clear();
    }

    fn check_port(&mut self) {
        let Some(port) = self.port.filter(|p| *p > 0) else {
            self.err("listen port unknown yet");
            return;
        };
        self.say(format!("testing port {port} with Soulseek's port checker…"));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(match crate::port::test(port).await {
                Ok(t) => Msg::PortTest(t.open, t.message),
                Err(e) => Msg::Error(format!("port test failed: {e:#}")),
            });
        });
    }

    pub fn unread_total(&self) -> u32 {
        self.convs.iter().map(|r| r.conv.un_acknowledged_message_count).sum()
    }

    // ---------- keys ----------

    /// Returns true to quit.
    fn on_key(&mut self, k: KeyEvent) -> bool {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match &mut self.mode {
            Mode::SearchInput => {
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let q = self.input.trim().to_string();
                        self.mode = Mode::Normal;
                        if !q.is_empty() {
                            self.query = q;
                            self.start_search();
                        }
                    }
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => self.input.clear(),
                    KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        let t = self.input.trim_end().rfind(' ').map(|i| i + 1).unwrap_or(0);
                        self.input.truncate(t);
                    }
                    KeyCode::Char(c) => self.input.push(c),
                    KeyCode::Down => {
                        self.mode = Mode::Normal;
                        self.move_sel(1);
                    }
                    _ => {}
                }
                return false;
            }
            Mode::OutputPrompt(buf) => {
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let v = buf.trim().to_string();
                        self.target = Target::from_opt(&self.cfg, (!v.is_empty()).then_some(v.as_str()));
                        self.mode = Mode::Normal;
                        let d = self.target.describe(&self.cfg);
                        self.say(format!("downloads go to {d}"));
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                return false;
            }
            Mode::ConfirmBan(user) => {
                let user = user.clone();
                self.mode = Mode::Normal;
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.ban(user);
                }
                return false;
            }
            Mode::UserDetail(user, _) => {
                let user = user.clone();
                self.mode = Mode::Normal;
                if k.code == KeyCode::Char('b') {
                    self.mode = Mode::ConfirmBan(user);
                }
                return false;
            }
            Mode::TypesPrompt(buf) => {
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let t = crate::quality::expand_types(&[buf.as_str()]);
                        self.mode = Mode::Normal;
                        self.types = (!t.is_empty()).then_some(t);
                        self.rerank();
                        match &self.types {
                            Some(t) => {
                                let shown = self.hits.len();
                                self.say(format!("showing only {} ({shown} results)", t.join(", ")))
                            }
                            None => self.say("file types: back to the preset's"),
                        }
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => buf.clear(),
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                return false;
            }
            Mode::Compose(_) | Mode::NewConversation(_) => {
                let composing = matches!(self.mode, Mode::Compose(_));
                let (Mode::Compose(buf) | Mode::NewConversation(buf)) = &mut self.mode else { unreachable!() };
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let text = std::mem::take(buf).trim().to_string();
                        self.mode = Mode::Normal;
                        if text.is_empty() {
                            return false;
                        }
                        if composing {
                            self.send_message(text);
                        } else {
                            self.thread_user = Some(text.clone());
                            self.thread.clear();
                            let _ = self.open_conv.send(Some(text));
                            self.mode = Mode::Compose(String::new());
                        }
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => buf.clear(),
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                return false;
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                return false;
            }
            Mode::Normal => {}
        }

        let prev = self.tab;
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(c @ '1'..='5') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Char('P') => self.check_port(),
            KeyCode::Tab => self.tab = self.tab.cycle(1),
            KeyCode::BackTab => self.tab = self.tab.cycle(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(15),
            KeyCode::PageUp => self.move_sel(-15),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
            _ => match self.tab {
                Tab::Search => self.search_key(k),
                Tab::Downloads => match k.code {
                    KeyCode::Char('c') => self.cancel_selected(),
                    KeyCode::Char('x') => self.clear_finished(),
                    KeyCode::Char('R') | KeyCode::Char('r') => self.retry_selected(),
                    KeyCode::Char('h') => self.toggle_hide(),
                    _ => {}
                },
                Tab::Uploads => match k.code {
                    KeyCode::Char('c') => self.cancel_selected(),
                    KeyCode::Char('x') => self.clear_finished(),
                    KeyCode::Char('h') => self.toggle_hide(),
                    KeyCode::Char('b') => {
                        if let Some(t) = self.selected_transfer() {
                            self.mode = Mode::ConfirmBan(t.username);
                        }
                    }
                    KeyCode::Enter => self.open_user_detail(),
                    _ => {}
                },
                Tab::History => {
                    if k.code == KeyCode::Char('u') {
                        self.unban_selected();
                    }
                }
                Tab::Messages => match k.code {
                    KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('i') => {
                        if self.thread_user.is_some() {
                            self.mode = Mode::Compose(String::new());
                        }
                    }
                    KeyCode::Char('n') => self.mode = Mode::NewConversation(String::new()),
                    KeyCode::Char('d') => self.close_selected_conversation(),
                    KeyCode::Char('b') => {
                        if let Some(u) = self.thread_user.clone() {
                            self.mode = Mode::ConfirmBan(u);
                        }
                    }
                    _ => {}
                },
            },
        }
        if self.tab == Tab::History && prev != Tab::History {
            self.refresh_history();
        }
        if self.tab == Tab::Messages {
            self.sync_open_conversation();
        } else if prev == Tab::Messages {
            // Leaving the tab: stop marking incoming messages as read.
            let _ = self.open_conv.send(None);
            self.thread_user = None;
        }
        false
    }

    fn toggle_hide(&mut self) {
        self.hide_old = !self.hide_old;
        self.rebuild_views();
        self.say(if self.hide_old { "hiding finished transfers older than 10 min" } else { "showing all transfers" });
    }

    fn search_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('/') | KeyCode::Char('i') => self.mode = Mode::SearchInput,
            KeyCode::Esc => self.mode = Mode::SearchInput,
            KeyCode::Enter => self.download_selected(false),
            KeyCode::Char('a') | KeyCode::Char('d') => self.download_selected(true),
            KeyCode::Char('f') => {
                if !self.preset_names.is_empty() {
                    self.preset = (self.preset + 1) % self.preset_names.len();
                    self.rerank();
                    let name = self.preset_names[self.preset].clone();
                    self.say(format!("filter: {name} ({})", self.filter().describe()));
                }
            }
            KeyCode::Char('t') => {
                let cur = self.types.as_ref().map(|t| t.join(", ")).unwrap_or_default();
                self.mode = Mode::TypesPrompt(cur);
            }
            KeyCode::Char('v') => {
                self.folders_view = !self.folders_view;
                let n = self.list_len(Tab::Search);
                self.tables[0].select(if n > 0 { Some(0) } else { None });
            }
            KeyCode::Char('s') => {
                self.sort = (self.sort + 1) % SORTS.len();
                self.rerank();
                self.say(format!("sort: {}", SORTS[self.sort]));
            }
            KeyCode::Char('r') => self.start_search(),
            KeyCode::Char('o') => {
                let cur = match &self.target {
                    Target::Dir(p) => crate::config::tilde(p),
                    Target::Default => String::new(),
                };
                self.mode = Mode::OutputPrompt(cur);
            }
            _ => {}
        }
    }

    /// Distinct users with unfinished uploads.
    pub fn active_uploaders(&self) -> usize {
        self.uploads.iter().filter(|t| !t.is_finished()).map(|t| t.username.as_str()).collect::<HashSet<_>>().len()
    }

    pub fn downloads_raw(&self) -> &[Transfer] {
        &self.downloads
    }

    pub fn uploads_raw(&self) -> &[Transfer] {
        &self.uploads
    }
}
