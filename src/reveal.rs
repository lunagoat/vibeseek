//! "Open in Dolphin": work out where a transfer's file is on disk and show it in the file
//! manager with the file selected.

use anyhow::{bail, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::api::{self, Share, Transfer};
use crate::config::{tilde, Config};
use crate::history::History;

/// An upload's remote path starts with the name of the share it's in
/// (`Share\Artist\Album\song.flac`). None if no share matches (e.g. it was un-shared since).
pub fn upload_path(shares: &[Share], t: &Transfer) -> Option<PathBuf> {
    let remote = t.filename.replace('/', "\\");
    shares
        .iter()
        .filter_map(|s| {
            let rest = remote.strip_prefix(&s.remote_path.replace('/', "\\"))?.strip_prefix('\\')?;
            Some((s.remote_path.len(), Path::new(&s.local_path).join(rest.replace('\\', "/"))))
        })
        // "Music" and "Music (old)" can both be shares: the longest name that fits wins.
        .max_by_key(|(len, _)| *len)
        .map(|(_, p)| p)
}

/// Where a download ends up: the folder it was queued into, or slskd's default layout
/// (downloads dir / the source folder's name).
pub fn download_path(cfg: &Config, t: &Transfer) -> PathBuf {
    let recorded = t.batch_id.and_then(|b| History::open().and_then(|h| h.destination(b)).ok().flatten());
    let dir = recorded.unwrap_or_else(|| cfg.downloads_dir().join(api::basename(api::dirname(&t.filename))));
    dir.join(t.basename())
}

/// Show `file` in Dolphin, selected. If the file isn't there (not downloaded yet, renamed,
/// moved) its folder is opened instead. Returns a line for the status bar.
pub fn show(file: &Path) -> Result<String> {
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if file.exists() {
        let fm = launch(file, true)?;
        return Ok(format!("showing {name} in {fm}"));
    }
    match file.parent().filter(|d| d.is_dir()) {
        Some(dir) => {
            let fm = launch(dir, false)?;
            Ok(format!("{name} isn't there — opened its folder in {fm}"))
        }
        None => bail!("not on disk: {}", tilde(file)),
    }
}

/// Dolphin if it's installed, otherwise whatever opens folders. Returns which one.
fn launch(path: &Path, select: bool) -> Result<&'static str> {
    let mut dolphin = Command::new("dolphin");
    if select {
        dolphin.arg("--select");
    }
    match spawn_detached(dolphin.arg(path)) {
        Ok(()) => return Ok("Dolphin"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail!("couldn't start Dolphin: {e}"),
    }
    // xdg-open can't select a file, so open the folder it's in.
    let dir = if select { path.parent().unwrap_or(path) } else { path };
    match spawn_detached(Command::new("xdg-open").arg(dir)) {
        Ok(()) => Ok("your file manager"),
        Err(e) => bail!("no file manager found (tried dolphin and xdg-open): {e}"),
    }
}

/// Start a GUI program that outlives us and can't scribble on the terminal.
fn spawn_detached(cmd: &mut Command) -> std::io::Result<()> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn()?;
    // Reap it whenever it exits, so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(remote: &str, local: &str) -> Share {
        Share { remote_path: remote.into(), local_path: local.into() }
    }

    fn upload(filename: &str) -> Transfer {
        serde_json::from_value(serde_json::json!({ "id": uuid::Uuid::nil(), "username": "u", "filename": filename })).unwrap()
    }

    #[test]
    fn upload_maps_through_its_share() {
        let shares = [share("FLACs", "/mnt/hdd/FLACs"), share("FLACs (no foobar)", "/mnt/hdd/FLACs (no foobar)")];
        let p = |f: &str| upload_path(&shares, &upload(f));
        assert_eq!(p("FLACs\\Artist\\Album\\01.flac"), Some("/mnt/hdd/FLACs/Artist/Album/01.flac".into()));
        // A share whose name merely starts the same isn't the match.
        assert_eq!(p("FLACs (no foobar)\\a.flac"), Some("/mnt/hdd/FLACs (no foobar)/a.flac".into()));
        assert_eq!(p("Elsewhere\\a.flac"), None);
    }
}
