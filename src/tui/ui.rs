//! Drawing the TUI.

use chrono::Local;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use std::collections::BTreeMap;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

use super::{App, Mode, Tab, SORTS};
use crate::api::{self, Transfer};
use crate::fmt;
use crate::quality::{self, Folder};

const ACCENT: Color = Color::Cyan;
const QUALITY: Color = Color::Magenta;
const DIM: Color = Color::DarkGray;

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn block(title: impl Into<String>) -> Block<'static> {
    let t: String = title.into();
    let b = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(dim());
    if t.is_empty() { b } else { b.title(Span::styled(format!(" {t} "), Style::default().fg(ACCENT).bold())) }
}

fn header(cols: &[&str]) -> Row<'static> {
    Row::new(cols.iter().map(|c| Cell::from(c.to_string()))).style(dim().add_modifier(Modifier::BOLD))
}

fn highlight() -> Style {
    Style::default().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD)
}

pub fn render(f: &mut Frame, a: &mut App) {
    let area = f.area();
    let [top, body, bottom] = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    render_top(f, a, top);
    match a.tab {
        Tab::Search => render_search(f, a, body),
        Tab::Downloads => render_transfers(f, a, body, false),
        Tab::Uploads => render_transfers(f, a, body, true),
        Tab::History => render_history(f, a, body),
        Tab::Messages => render_messages(f, a, body),
    }
    render_bottom(f, a, bottom);
    render_popup(f, a, area);
}

fn render_top(f: &mut Frame, a: &App, r: Rect) {
    let mut left = vec![Span::styled(" vibeseek ", Style::default().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    let unread = a.unread_total();
    let tabs = [(Tab::Search, "Search"), (Tab::Downloads, "Downloads"), (Tab::Uploads, "Uploads"), (Tab::History, "History"), (Tab::Messages, "Messages")];
    for (i, (t, name)) in tabs.iter().enumerate() {
        let label = if *t == Tab::Messages && unread > 0 { format!(" {} {name} ({unread}) ", i + 1) } else { format!(" {} {name} ", i + 1) };
        left.push(if a.tab == *t { Span::styled(label, Style::default().fg(ACCENT).bold().underlined()) } else { Span::styled(label, dim()) });
    }

    let mut right = vec![];
    match (&a.offline, &a.app) {
        (Some(_), _) | (None, None) => right.push(Span::styled("● slskd offline — vibeseek daemon start ", Style::default().fg(Color::Red))),
        (None, Some(s)) => {
            if s.server.is_logged_in {
                right.push(Span::styled(format!("● {}", s.user.username), Style::default().fg(Color::Green)));
            } else {
                right.push(Span::styled(format!("● {}", s.server.state), Style::default().fg(Color::Yellow)));
            }
            if let Some(p) = a.port {
                // Colored once a port test has run (P).
                let style = match &a.port_test {
                    Some((true, _)) => Style::default().fg(Color::Green),
                    Some((false, _)) => Style::default().fg(Color::Red),
                    None => dim(),
                };
                right.push(Span::styled(format!("  :{p}"), style));
            }
            let sh = &s.shares;
            let shares = if sh.scanning { format!("  scanning {:.0}%", sh.scan_progress * 100.0) } else { format!("  {} shared", sh.files) };
            right.push(Span::styled(shares, dim()));
        }
    }
    let ad = a.downloads_raw().iter().filter(|t| t.is_active()).count();
    let au = a.uploads_raw().iter().filter(|t| t.is_active()).count();
    if unread > 0 {
        right.push(Span::styled(format!("  ✉ {unread}"), Style::default().fg(Color::Yellow).bold()));
    }
    right.push(Span::styled(format!("  ↓{ad}"), Style::default().fg(if ad > 0 { ACCENT } else { DIM })));
    right.push(Span::styled(format!(" ↑{au} "), Style::default().fg(if au > 0 { Color::Green } else { DIM })));

    let rw: u16 = right.iter().map(|s| s.content.width() as u16).sum();
    let [l, rr] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(r);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(Line::from(right)), rr);
}

fn render_bottom(f: &mut Frame, a: &App, r: Rect) {
    let hints = match (&a.mode, a.tab) {
        (Mode::SearchInput, _) => "Enter search  Esc results  Ctrl-U clear",
        (Mode::OutputPrompt(_), _) => "Enter set folder (empty = default)  Esc cancel",
        (_, Tab::Search) => "/ search  Enter download  a whole folder  f filter  t file types  v files/folders  s sort  o output folder  ? help",
        (_, Tab::Downloads) => "o open in Dolphin  c cancel  R retry failed  x clear finished  h show/hide old  ? help",
        (_, Tab::Uploads) => "Enter user details  o open in Dolphin  b ban  c cancel  x clear finished  h hide/show old  ? help",
        (Mode::Compose(_), _) => "Enter send  Esc cancel  Ctrl-U clear",
        (Mode::TypesPrompt(_), _) => "Enter apply (empty = preset's types)  Esc cancel  Ctrl-U clear",
        (Mode::NewConversation(_), _) => "type a username, Enter to start writing  Esc cancel",
        (_, Tab::History) => "u unban selected  ↑↓ select ban  P check port  ? help",
        (_, Tab::Messages) => "Enter reply  n new message  b ban  d close conversation  P check port  ? help",
    };
    let mut spans = vec![];
    if let Some((msg, at, err)) = &a.status {
        if at.elapsed() < Duration::from_secs(6) {
            spans.push(Span::styled(format!(" {msg} "), Style::default().fg(if *err { Color::Red } else { Color::Green })));
            spans.push(Span::styled("│", dim()));
        }
    }
    spans.push(Span::styled(format!(" {hints}"), dim()));
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

// ---------- search ----------

fn peer_cell(user: &str, free: bool, queue: u64) -> Cell<'static> {
    if free {
        Cell::from(user.to_string()).style(Style::default().fg(Color::Green))
    } else if queue > 0 {
        Cell::from(format!("{user} q{queue}")).style(Style::default().fg(Color::Yellow))
    } else {
        Cell::from(user.to_string())
    }
}

fn folder_quality(f: &Folder) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for h in &f.files {
        *counts.entry(quality::quality_label(&h.file)).or_default() += 1;
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(q, _)| q).unwrap_or_default()
}

fn render_search(f: &mut Frame, a: &mut App, r: Rect) {
    let [input_r, info_r, table_r] = Layout::vertical([Constraint::Length(3), Constraint::Length(1), Constraint::Min(1)]).areas(r);

    let editing = matches!(a.mode, Mode::SearchInput);
    let input_block = block("Search").border_style(if editing { Style::default().fg(ACCENT) } else { dim() });
    let text = if a.input.is_empty() && !editing {
        Line::from(Span::styled("press / to search", dim()))
    } else {
        Line::from(a.input.as_str())
    };
    f.render_widget(Paragraph::new(text).block(input_block), input_r);
    if editing {
        f.set_cursor_position((input_r.x + 1 + a.input.width() as u16, input_r.y + 1));
    }

    let pname = a.preset_names.get(a.preset).cloned().unwrap_or_default();
    let mut info = vec![
        Span::styled(format!(" filter {pname} "), Style::default().fg(ACCENT)),
        Span::styled(format!("({})", a.filter().describe()), dim()),
        Span::styled(if a.types.is_some() { "  [t: types set]".to_string() } else { String::new() }, Style::default().fg(Color::Yellow)),
        Span::styled(format!("  · {} · sort {}", if a.folders_view { "folders" } else { "files" }, SORTS[a.sort]), dim()),
        Span::styled(format!("  · → {}", a.target.describe(&a.cfg)), dim()),
    ];
    if let Some(start) = a.searching {
        let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(start.elapsed().as_millis() / 100) as usize % 10];
        info.push(Span::styled(format!("  {spin} {} peers, {} files", a.progress.0, a.progress.1), Style::default().fg(Color::Yellow)));
    } else if a.hidden > 0 && !a.filter().is_empty() {
        info.push(Span::styled(format!("  · {} hidden by filter (f)", a.hidden), Style::default().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(info)), info_r);

    let w = table_r.width.saturating_sub(2) as usize;
    let title = if a.query.is_empty() { String::new() } else { format!("{} — {}", a.query, a.list_len(Tab::Search)) };
    let (table, _) = if a.folders_view {
        let path_w = w.saturating_sub(6 + 13 + 9 + 18 + 11 + 6).max(10);
        let rows: Vec<Row> = a
            .folders
            .iter()
            .map(|fo| {
                let h = &fo.files[0];
                Row::new(vec![
                    Cell::from(format!("{:>4}", fo.files.len())),
                    Cell::from(folder_quality(fo)).style(Style::default().fg(QUALITY)),
                    Cell::from(format!("{:>8}", fmt::size(fo.size()))),
                    peer_cell(&fmt::trunc(&fo.username, 17), h.free_slot, h.queue_length),
                    Cell::from(fmt::speed(h.upload_speed as f64)).style(dim()),
                    Cell::from(fmt::trunc_left(&fo.dir.replace('\\', "/"), path_w)),
                ])
            })
            .collect();
        let t = Table::new(rows, [Constraint::Length(5), Constraint::Length(13), Constraint::Length(9), Constraint::Length(18), Constraint::Length(11), Constraint::Min(10)])
            .header(header(&["files", "quality", "size", "peer", "speed", "folder"]));
        (t, 0)
    } else {
        let path_w = w.saturating_sub(13 + 9 + 6 + 18 + 11 + 6).max(10);
        let rows: Vec<Row> = a
            .hits
            .iter()
            .map(|h| {
                let path = h.file.filename.replace('\\', "/");
                let name = api::basename(&path).to_string();
                let dir = api::dirname(&path);
                // Show "…/folder/" dimmed and the filename bright.
                let name_w = name.width().min(path_w);
                let dir_w = path_w.saturating_sub(name_w + 1);
                let line = if dir_w > 3 {
                    Line::from(vec![Span::styled(format!("{}/", fmt::trunc_left(dir, dir_w)), dim()), Span::raw(fmt::trunc(&name, name_w))])
                } else {
                    Line::from(fmt::trunc_left(&name, path_w))
                };
                Row::new(vec![
                    Cell::from(quality::quality_label(&h.file)).style(Style::default().fg(QUALITY)),
                    Cell::from(format!("{:>8}", fmt::size(h.file.size))),
                    Cell::from(h.file.length.map(|l| fmt::duration(l as u64)).unwrap_or_default()),
                    peer_cell(&fmt::trunc(&h.username, 17), h.free_slot, h.queue_length),
                    Cell::from(fmt::speed(h.upload_speed as f64)).style(dim()),
                    Cell::from(line),
                ])
            })
            .collect();
        let t = Table::new(rows, [Constraint::Length(13), Constraint::Length(9), Constraint::Length(6), Constraint::Length(18), Constraint::Length(11), Constraint::Min(10)])
            .header(header(&["quality", "size", "len", "peer", "speed", "file"]));
        (t, 0)
    };
    let table = table.block(block(title)).row_highlight_style(highlight()).highlight_symbol("▌");
    f.render_stateful_widget(table, table_r, &mut a.tables[0]);

    if a.list_len(Tab::Search) == 0 && a.searching.is_none() {
        let msg = if a.query.is_empty() {
            "Type a search and press Enter.\n\nTips: 'artist album' finds folders — press v for folder view, Enter downloads the whole folder.\nf cycles quality filters (lossless → lossy-ok → any)."
        } else {
            "No results. Try f to loosen the filter, or different words."
        };
        let inner = Rect { x: table_r.x + 2, y: table_r.y + 2, width: table_r.width.saturating_sub(4), height: table_r.height.saturating_sub(3) };
        f.render_widget(Paragraph::new(msg).style(dim()).wrap(Wrap { trim: false }), inner);
    }
}

// ---------- transfers ----------

fn state_style(t: &Transfer) -> Style {
    Style::default().fg(if t.is_succeeded() {
        Color::Green
    } else if t.is_failed() {
        Color::Red
    } else if t.is_active() {
        ACCENT
    } else {
        Color::Yellow
    })
}

fn transfer_table(list: &[Transfer], w: usize, uploads: bool) -> Table<'static> {
    let name_w = w.saturating_sub(18 + 15 + 17 + 11 + 8 + 9 + 7).max(10);
    let rows: Vec<Row> = list
        .iter()
        .map(|t| {
            let st = state_style(t);
            let (speed, eta) = if t.is_active() {
                (fmt::speed(t.average_speed), fmt::eta(t.size.saturating_sub(t.bytes_transferred), t.average_speed))
            } else {
                (String::new(), String::new())
            };
            Row::new(vec![
                Cell::from(fmt::trunc(&t.username, 17)),
                Cell::from(fmt::trunc(&t.short_state(), 14)).style(st),
                Cell::from(Line::from(vec![Span::styled(fmt::bar(t.percent_complete, 10), st), Span::raw(format!(" {:>3.0}%", t.percent_complete))])),
                Cell::from(speed),
                Cell::from(eta).style(dim()),
                Cell::from(format!("{:>8}", fmt::size(t.size))),
                Cell::from(fmt::trunc(t.basename(), name_w)),
            ])
        })
        .collect();
    Table::new(
        rows,
        [Constraint::Length(18), Constraint::Length(15), Constraint::Length(16), Constraint::Length(11), Constraint::Length(8), Constraint::Length(9), Constraint::Min(10)],
    )
    .header(header(&[if uploads { "downloader" } else { "peer" }, "state", "progress", "speed", "eta", "size", "file"]))
    .row_highlight_style(highlight())
    .highlight_symbol("▌")
}

fn render_transfers(f: &mut Frame, a: &mut App, r: Rect, uploads: bool) {
    let list = if uploads { &a.ul_view } else { &a.dl_view };
    let active: Vec<&Transfer> = list.iter().filter(|t| t.is_active()).collect();
    let queued = list.iter().filter(|t| t.is_queued()).count();
    let speed: f64 = active.iter().map(|t| t.average_speed).sum();

    let [sum_r, table_r] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(r);
    let big = |s: String, c: Color| Span::styled(s, Style::default().fg(c).bold());
    let mut spans = vec![
        big(format!(" {}", active.len()), ACCENT),
        Span::styled(" active  ", dim()),
        big(queued.to_string(), Color::Yellow),
        Span::styled(" queued  ", dim()),
        big(fmt::speed(speed), Color::Green),
        Span::styled("  ", dim()),
    ];
    if uploads {
        spans.push(big(a.active_uploaders().to_string(), Color::White));
        spans.push(Span::styled(" people downloading from you   today: ", dim()));
        spans.push(big(format!("{} files", a.hist.today.files), Color::White));
        spans.push(Span::styled(" / ", dim()));
        spans.push(big(fmt::size(a.hist.today.bytes), Color::White));
        spans.push(Span::styled(" served", dim()));
    }
    if !a.hide_old {
        spans.push(Span::styled("   (showing old)", dim()));
    }
    let title = if uploads { "Uploads — people downloading from you" } else { "Downloads" };
    f.render_widget(Paragraph::new(Line::from(spans)).block(block(title)), sum_r);

    let empty = list.is_empty();
    let t = transfer_table(list, table_r.width.saturating_sub(3) as usize, uploads).block(block(""));
    let idx = if uploads { 2 } else { 1 };
    f.render_stateful_widget(t, table_r, &mut a.tables[idx]);
    if empty {
        let msg = if uploads { "Nobody is downloading from you right now." } else { "No downloads. Find something in the Search tab (1)." };
        let inner = Rect { x: table_r.x + 2, y: table_r.y + 2, width: table_r.width.saturating_sub(4), height: 1 };
        f.render_widget(Paragraph::new(msg).style(dim()), inner);
    }
}

// ---------- history ----------

fn render_history(f: &mut Frame, a: &mut App, r: Rect) {
    let h = &a.hist;
    let [tot_r, mid_r, bot_r] = Layout::vertical([Constraint::Length(3), Constraint::Percentage(45), Constraint::Min(5)]).areas(r);
    let stat = |label: &str, t: &crate::history::Totals| {
        vec![
            Span::styled(format!(" {label} "), dim()),
            Span::styled(format!("{} files", t.files), Style::default().bold()),
            Span::styled(" · ", dim()),
            Span::styled(fmt::size(t.bytes), Style::default().fg(Color::Green).bold()),
            Span::styled(format!(" · {} people   ", t.users), dim()),
        ]
    };
    let mut spans = stat("today", &h.today);
    spans.extend(stat("7 days", &h.week));
    spans.extend(stat("all time", &h.all));
    f.render_widget(Paragraph::new(Line::from(spans)).block(block("Upload stats")), tot_r);

    let wide = mid_r.width >= 100;
    let dir = if wide { Direction::Horizontal } else { Direction::Vertical };
    let [users_r, files_r] = Layout::default().direction(dir).constraints([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(mid_r);

    let users: Vec<Row> = h
        .users
        .iter()
        .map(|(u, n, b)| Row::new(vec![Cell::from(u.clone()), Cell::from(format!("{n:>5}")), Cell::from(format!("{:>9}", fmt::size(*b)))]))
        .collect();
    f.render_widget(
        Table::new(users, [Constraint::Min(10), Constraint::Length(6), Constraint::Length(10)]).header(header(&["user", "files", "data"])).block(block("Top downloaders")),
        users_r,
    );
    let fw = files_r.width.saturating_sub(16) as usize;
    let files: Vec<Row> = h
        .files
        .iter()
        .map(|(p, n, u)| Row::new(vec![Cell::from(fmt::trunc_left(&p.replace('\\', "/"), fw)), Cell::from(format!("{n:>5}")), Cell::from(format!("{u:>6}"))]))
        .collect();
    f.render_widget(
        Table::new(files, [Constraint::Min(10), Constraint::Length(6), Constraint::Length(7)]).header(header(&["file", "times", "people"])).block(block("Most downloaded")),
        files_r,
    );

    let [recent_r, bans_r] = Layout::horizontal([Constraint::Min(40), Constraint::Length(28)]).areas(bot_r);
    let rw = recent_r.width.saturating_sub(12 + 18 + 12 + 9 + 6) as usize;
    let recent: Vec<Row> = h
        .recent
        .iter()
        .map(|x| {
            let ok = x.state.ends_with("Succeeded");
            let when = chrono::DateTime::parse_from_rfc3339(&x.ended_at).map(|d| d.with_timezone(&Local).format("%b %d %H:%M").to_string()).unwrap_or_default();
            Row::new(vec![
                Cell::from(when).style(dim()),
                Cell::from(fmt::trunc(&x.username, 17)),
                Cell::from(x.state.trim_start_matches("Completed, ").to_string()).style(Style::default().fg(if ok { Color::Green } else { Color::Red })),
                Cell::from(format!("{:>8}", fmt::size(x.size))),
                Cell::from(fmt::trunc(api::basename(&x.filename), rw.max(10))),
            ])
        })
        .collect();
    let recent_block = if h.recent.is_empty() { block("Recent uploads — none recorded yet") } else { block("Recent uploads") };
    f.render_widget(
        Table::new(recent, [Constraint::Length(12), Constraint::Length(18), Constraint::Length(12), Constraint::Length(9), Constraint::Min(10)])
            .header(header(&["when", "user", "result", "size", "file"]))
            .block(recent_block),
        recent_r,
    );
    let bans: Vec<Row> = h.bans.iter().map(|b| Row::new(vec![Cell::from(b.clone())])).collect();
    let bans_empty = bans.is_empty();
    f.render_stateful_widget(
        Table::new(bans, [Constraint::Min(5)]).block(block("Banned (u unban)")).row_highlight_style(highlight()).highlight_symbol("▌"),
        bans_r,
        &mut a.tables[3],
    );
    if bans_empty {
        f.render_widget(Paragraph::new(" nobody").style(dim()), Rect { x: bans_r.x + 1, y: bans_r.y + 1, width: bans_r.width.saturating_sub(2), height: 1 });
    }
}

// ---------- messages ----------

/// Hard-wrap text to `width` display columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = vec![];
    for para in text.split('\n') {
        let mut line = String::new();
        let mut w = 0;
        for word in para.split(' ') {
            let ww = word.width();
            if w > 0 && w + 1 + ww > width {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            if ww > width {
                // A single overlong word: break it by characters.
                for ch in word.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if w + cw > width {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                    line.push(ch);
                    w += cw;
                }
                continue;
            }
            if w > 0 {
                line.push(' ');
                w += 1;
            }
            line.push_str(word);
            w += ww;
        }
        out.push(line);
    }
    out
}

fn render_messages(f: &mut Frame, a: &mut App, r: Rect) {
    let [list_r, thread_r] = Layout::horizontal([Constraint::Length(30), Constraint::Min(20)]).areas(r);

    let rows: Vec<Row> = a
        .convs
        .iter()
        .map(|c| {
            let unread = c.conv.un_acknowledged_message_count;
            let when = c.last.as_ref().and_then(|m| m.timestamp).map(fmt_ago).unwrap_or_default();
            let name = if unread > 0 {
                Cell::from(Line::from(vec![Span::styled(fmt::trunc(&c.conv.username, 18), Style::default().bold()), Span::styled(format!(" {unread}"), Style::default().fg(Color::Yellow).bold())]))
            } else {
                Cell::from(fmt::trunc(&c.conv.username, 20))
            };
            Row::new(vec![name, Cell::from(when).style(dim())])
        })
        .collect();
    let empty = rows.is_empty();
    f.render_stateful_widget(
        Table::new(rows, [Constraint::Min(10), Constraint::Length(5)]).block(block("Conversations")).row_highlight_style(highlight()).highlight_symbol("▌"),
        list_r,
        &mut a.tables[Tab::Messages.index()],
    );
    if empty {
        f.render_widget(Paragraph::new(" none yet — n to write").style(dim()), Rect { x: list_r.x + 1, y: list_r.y + 1, width: list_r.width.saturating_sub(2), height: 1 });
    }

    let composing = matches!(a.mode, Mode::Compose(_));
    let [msgs_r, input_r] = Layout::vertical([Constraint::Min(3), Constraint::Length(if composing { 3 } else { 0 })]).areas(thread_r);
    let Some(user) = a.thread_user.clone() else {
        f.render_widget(Paragraph::new("\n  Select a conversation, or press n to message someone.").style(dim()).block(block("")), msgs_r);
        return;
    };
    let inner_w = msgs_r.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = vec![];
    let mut last_day = String::new();
    for m in &a.thread {
        let local = m.timestamp.map(|t| t.with_timezone(&Local));
        let day = local.map(|t| t.format("%A, %b %d").to_string()).unwrap_or_default();
        if day != last_day {
            lines.push(Line::from(Span::styled(format!("── {day} ──"), dim())));
            last_day = day;
        }
        let (who, color) = if m.is_incoming() { (user.as_str(), ACCENT) } else { ("you", QUALITY) };
        let time = local.map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
        lines.push(Line::from(vec![Span::styled(format!("{time} "), dim()), Span::styled(who.to_string(), Style::default().fg(color).bold())]));
        for l in wrap(&m.message, inner_w.saturating_sub(2)) {
            lines.push(Line::from(format!("  {l}")));
        }
    }
    if a.thread.is_empty() {
        lines.push(Line::from(Span::styled("no messages yet — Enter to write one", dim())));
    }
    // Stick to the bottom (newest messages).
    let height = msgs_r.height.saturating_sub(2) as usize;
    let scroll = lines.len().saturating_sub(height) as u16;
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block(user.clone())), msgs_r);

    if let Mode::Compose(buf) = &a.mode {
        // Show the tail of long drafts.
        let w = input_r.width.saturating_sub(2) as usize;
        let shown = fmt::trunc_left(buf, w.saturating_sub(1));
        f.render_widget(Paragraph::new(shown.as_str()).block(block(format!("To {user}")).border_style(Style::default().fg(ACCENT))), input_r);
        f.set_cursor_position((input_r.x + 1 + shown.width() as u16, input_r.y + 1));
    }
}

/// "5m", "3h", "2d".
fn fmt_ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let s = (chrono::Utc::now() - t).num_seconds().max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

// ---------- popups ----------

fn centered(r: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(r.width.saturating_sub(2));
    let h = h.min(r.height.saturating_sub(2));
    Rect::new(r.x + (r.width - w) / 2, r.y + (r.height - h) / 2, w, h)
}

fn render_popup(f: &mut Frame, a: &App, area: Rect) {
    match &a.mode {
        Mode::Help => {
            let lines = [
                ("Anywhere", ""),
                ("1-4 / Tab", "switch tabs"),
                ("↑↓ j k PgUp PgDn g G", "move"),
                ("q / Ctrl-C", "quit"),
                ("", ""),
                ("Search", ""),
                ("/  i", "type a search (Enter runs it)"),
                ("Enter", "download selected file (or folder in folder view)"),
                ("a", "download the selected file's whole folder"),
                ("v", "toggle files / folders (albums) view"),
                ("f", "cycle filter preset (lossless, lossy-ok, any, video, dsd…)"),
                ("t", "only show certain file types: mkv, dsf, video, dsd…"),
                ("s", "cycle sort (best, size, speed, peer)"),
                ("o", "set the download folder"),
                ("r", "re-run the search"),
                ("", ""),
                ("Downloads / Uploads", ""),
                ("o", "show the selected file in Dolphin"),
                ("c", "cancel selected"),
                ("x", "clear finished from the list (asks first; uploads stay in history)"),
                ("h", "show / hide finished transfers older than 10 min"),
                ("R", "retry failed download"),
                ("Enter (uploads)", "user details + their history"),
                ("b (uploads)", "ban user (cancels their uploads)"),
                ("", ""),
                ("History", ""),
                ("u", "unban selected user"),
                ("", ""),
                ("Messages", ""),
                ("Enter / r", "reply to the selected conversation"),
                ("n", "message someone new"),
                ("b / d", "ban the user / close the conversation"),
                ("P (anywhere)", "check your port with Soulseek's port tester"),
            ];
            let text: Vec<Line> = lines
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        Line::from(Span::styled(k.to_string(), Style::default().fg(ACCENT).bold()))
                    } else {
                        Line::from(vec![Span::styled(format!("  {k:<22}"), Style::default().bold()), Span::styled(v.to_string(), dim())])
                    }
                })
                .collect();
            let r = centered(area, 76, text.len() as u16 + 2);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(text).block(block("Keys — any key closes")), r);
        }
        Mode::OutputPrompt(buf) => {
            let r = centered(area, 70, 6);
            f.render_widget(Clear, r);
            let text = vec![
                Line::from(Span::styled("Download folder (empty = slskd default, ~ works):", dim())),
                Line::from(""),
                Line::from(buf.as_str()),
            ];
            f.render_widget(Paragraph::new(text).block(block("Output folder")), r);
            f.set_cursor_position((r.x + 1 + buf.width() as u16, r.y + 3));
        }
        Mode::ConfirmClear(uploads, n) => {
            let what = if *uploads { "uploads" } else { "downloads" };
            let r = centered(area, 78, 6);
            f.render_widget(Clear, r);
            let note = if *uploads {
                "They stay in your upload history (tab 4) and `vibeseek uploads --all`."
            } else {
                "Files you downloaded are not touched."
            };
            let text = vec![
                Line::from(vec![Span::raw(format!("Clear {n} finished {what} from this list?"))]),
                Line::from(Span::styled(note, dim())),
                Line::from(Span::styled("Active and queued transfers stay.   y / n", dim())),
            ];
            f.render_widget(Paragraph::new(text).block(block(format!("Clear {what}"))), r);
        }
        Mode::ConfirmBan(user) => {
            let r = centered(area, 56, 5);
            f.render_widget(Clear, r);
            let text = vec![
                Line::from(vec![Span::raw("Ban "), Span::styled(user.clone(), Style::default().fg(Color::Red).bold()), Span::raw("?")]),
                Line::from(Span::styled("Cancels their uploads and blocks future ones.  y / n", dim())),
            ];
            f.render_widget(Paragraph::new(text).block(block("Ban user")), r);
        }
        Mode::UserDetail(user, rows) => {
            let current: Vec<&Transfer> = a.uploads_raw().iter().filter(|t| &t.username == user).collect();
            let total: u64 = rows.iter().filter(|x| x.state.ends_with("Succeeded")).map(|x| x.bytes).sum();
            let r = centered(area, 100, 30);
            f.render_widget(Clear, r);
            let [head, cur_r, hist_r] =
                Layout::vertical([Constraint::Length(2), Constraint::Length(current.len().min(8) as u16 + 3), Constraint::Min(3)]).areas(block("").inner(r));
            f.render_widget(block(format!("{user} — b ban, any key closes")), r);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {} current transfer(s)", current.len()), Style::default().bold()),
                    Span::styled(format!("   history: {} uploads, {} served", rows.len(), fmt::size(total)), dim()),
                ])),
                head,
            );
            let cur: Vec<Transfer> = current.into_iter().cloned().collect();
            f.render_widget(transfer_table(&cur, cur_r.width as usize, true).block(block("Now")), cur_r);
            let w = hist_r.width.saturating_sub(12 + 12 + 9 + 4) as usize;
            let hist: Vec<Row> = rows
                .iter()
                .map(|x| {
                    let ok = x.state.ends_with("Succeeded");
                    Row::new(vec![
                        Cell::from(chrono::DateTime::parse_from_rfc3339(&x.ended_at).map(|d| d.with_timezone(&Local).format("%b %d %H:%M").to_string()).unwrap_or_default()).style(dim()),
                        Cell::from(x.state.trim_start_matches("Completed, ").to_string()).style(Style::default().fg(if ok { Color::Green } else { Color::Red })),
                        Cell::from(format!("{:>8}", fmt::size(x.size))),
                        Cell::from(fmt::trunc_left(&x.filename.replace('\\', "/"), w.max(10))),
                    ])
                })
                .collect();
            f.render_widget(
                Table::new(hist, [Constraint::Length(12), Constraint::Length(12), Constraint::Length(9), Constraint::Min(10)]).block(block("Past uploads")),
                hist_r,
            );
        }
        Mode::TypesPrompt(buf) => {
            let r = centered(area, 72, 8);
            f.render_widget(Clear, r);
            let text = vec![
                Line::from(Span::styled("Extensions or groups, comma separated. Empty = the preset's types.", dim())),
                Line::from(Span::styled("e.g.  mkv  ·  dsd  ·  video  ·  flac, dsf", dim())),
                Line::from(Span::styled("groups: video, dsd, lossless, lossy, audio", dim())),
                Line::from(""),
                Line::from(buf.as_str()),
            ];
            f.render_widget(Paragraph::new(text).block(block("File types")), r);
            f.set_cursor_position((r.x + 1 + buf.width() as u16, r.y + 5));
        }
        Mode::NewConversation(buf) => {
            let r = centered(area, 60, 5);
            f.render_widget(Clear, r);
            let text = vec![Line::from(Span::styled("Send a message to (Soulseek username):", dim())), Line::from(buf.as_str())];
            f.render_widget(Paragraph::new(text).block(block("New message")), r);
            f.set_cursor_position((r.x + 1 + buf.width() as u16, r.y + 2));
        }
        Mode::Normal | Mode::SearchInput | Mode::Compose(_) => {}
    }
}
