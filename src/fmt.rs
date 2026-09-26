//! Human-readable formatting helpers.

use unicode_width::UnicodeWidthChar;

pub fn size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else if v >= 100.0 { format!("{v:.0} {}", U[i]) } else { format!("{v:.1} {}", U[i]) }
}

pub fn speed(bps: f64) -> String {
    if bps <= 0.0 { "-".into() } else { format!("{}/s", size(bps as u64)) }
}

/// Seconds → "3:47" or "1:02:03".
pub fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

pub fn eta(remaining: u64, bps: f64) -> String {
    if bps <= 0.0 { "-".into() } else { duration((remaining as f64 / bps) as u64) }
}

/// Truncate to a display width, keeping the *end* (the most specific part of a path).
pub fn trunc_left(s: &str, width: usize) -> String {
    let w: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if w <= width {
        return s.to_string();
    }
    let mut out = vec![];
    let mut used = 1; // for the ellipsis
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if used + cw > width {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.reverse();
    format!("…{}", out.into_iter().collect::<String>())
}

/// Truncate to a display width, keeping the start.
pub fn trunc(s: &str, width: usize) -> String {
    let w: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if w <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > width {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// A text progress bar like "████▌     ".
pub fn bar(pct: f64, width: usize) -> String {
    let pct = pct.clamp(0.0, 100.0) / 100.0;
    let cells = pct * width as f64;
    let full = cells.floor() as usize;
    let half = cells - full as f64 >= 0.5;
    let mut s = "█".repeat(full);
    if half && full < width {
        s.push('▌');
    }
    let len = s.chars().count();
    s.push_str(&" ".repeat(width.saturating_sub(len)));
    s
}
