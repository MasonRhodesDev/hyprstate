//! The wayland idle-inhibitor check: last "Inhibit locks: N" from hypridle.
//! There is no query protocol for idle-inhibit. hypridle's systemd unit logs
//! to the journal. A leftover `~/.config/hypr/logs/hypridle.log` redirect is
//! not consulted (it can be months stale). Journal only, with a health signal
//! so a format change is loud instead of silently "no inhibitor". Logind
//! inhibitors are a separate, preferred source in the daemon poller.

use std::process::Command;

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

/// (inhibitor active, parse health). Last "Inhibit locks: N" wins; N > 0 = active.
pub fn wayland_inhibitor_active() -> (bool, ParseHealth) {
    let Some(text) = journal_tail() else {
        return (false, ParseHealth::ReadError);
    };
    if text.is_empty() {
        return (false, ParseHealth::LogMissing);
    }
    match parse_inhibit_locks(&text) {
        Some(n) => (n > 0, ParseHealth::Ok),
        None => (false, ParseHealth::NoMarkerFound),
    }
}

fn journal_tail() -> Option<String> {
    // journalctl defaults to newest-first; --reverse so the last line is the
    // newest marker and parse_inhibit_locks (last wins) matches prior
    // file-tail semantics. Without this, a held Chromium lock looks like 0.
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
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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
