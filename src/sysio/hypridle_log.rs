//! The wayland idle-inhibitor check: last "Inhibit locks: N" from hypridle.
//! There is no query protocol for idle-inhibit. hypridle's systemd unit logs
//! to the journal; `~/.config/hypr/logs/hypridle.log` is only a leftover
//! redirect and can be months stale. Journal first, file as fallback, with a
//! health signal so a format change is loud instead of silently "no inhibitor".

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Command;

pub const TAIL_BYTES: u64 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseHealth {
    Ok,
    LogMissing,
    NoMarkerFound,
    ReadError,
}

impl ParseHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            ParseHealth::Ok => "ok",
            ParseHealth::LogMissing => "log-missing",
            ParseHealth::NoMarkerFound => "no-marker-found",
            ParseHealth::ReadError => "read-error",
        }
    }
}

pub fn hypridle_log_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".config/hypr/logs/hypridle.log")
}

/// (inhibitor active, parse health). Last "Inhibit locks: N" wins; N > 0 = active.
pub fn wayland_inhibitor_active() -> (bool, ParseHealth) {
    if let Some(n) = parse_inhibit_locks(&journal_tail()) {
        return (n > 0, ParseHealth::Ok);
    }
    from_file()
}

fn from_file() -> (bool, ParseHealth) {
    let path = hypridle_log_path();
    if !path.exists() {
        return (false, ParseHealth::LogMissing);
    }
    let tail = match read_tail(&path) {
        Ok(t) => t,
        Err(_) => return (false, ParseHealth::ReadError),
    };
    match parse_inhibit_locks(&tail) {
        Some(n) => (n > 0, ParseHealth::Ok),
        None => (false, ParseHealth::NoMarkerFound),
    }
}

fn journal_tail() -> String {
    // journalctl defaults to newest-first; --reverse so the last line is the
    // newest marker and parse_inhibit_locks (last wins) matches file-tail
    // semantics. Without this, a held Chromium lock looks like 0.
    let out = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            "hypridle",
            "-g",
            "(?i)inhibit locks",
            "-n",
            "20",
            "--reverse",
            "--output=cat",
            "--no-pager",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}

/// Last "Inhibit locks: N" / "inhibit locks: N" in `text`.
pub fn parse_inhibit_locks(text: &str) -> Option<u64> {
    let mut latest = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(idx) = lower.find("inhibit locks:") else {
            continue;
        };
        let rest = line[idx + "inhibit locks:".len()..].trim_start();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            latest = Some(n);
        }
    }
    latest
}

fn read_tail(path: &std::path::Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let size = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_inhibit_locks;

    #[test]
    fn last_inhibit_locks_wins() {
        let text = "[LOG] Inhibit locks: 1\n[LOG] Inhibit locks: 0\n";
        assert_eq!(parse_inhibit_locks(text), Some(0));
    }

    #[test]
    fn chromium_screensaver_lock_is_held() {
        let text = "[LOG] ScreenSaver inhibit: true dbus message from /usr/lib/chromium/chromium (owner: :1.240) with content Video Wake Lock\n[LOG] Inhibit locks: 1\n";
        assert_eq!(parse_inhibit_locks(text), Some(1));
    }

    #[test]
    fn on_idled_lowercase_marker() {
        let text = "[LOG] Ignoring from onIdled(), inhibit locks: 1\n";
        assert_eq!(parse_inhibit_locks(text), Some(1));
    }

    #[test]
    fn no_marker() {
        assert_eq!(parse_inhibit_locks("nothing here"), None);
    }
}
