//! The wayland idle-inhibitor check: tail hypridle's log for the last
//! "Inhibit locks: N" marker. There is no query protocol for idle-inhibit,
//! so this fragile mechanism is kept from v1 — but isolated here with an
//! explicit health signal so a hypridle log-format change or missing
//! redirect is loud instead of silently reading "no inhibitor" forever.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

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

/// (inhibitor active, parse health). Last "Inhibit locks: N" in the final
/// 8 KiB wins; N > 0 = active.
pub fn wayland_inhibitor_active() -> (bool, ParseHealth) {
    let path = hypridle_log_path();
    if !path.exists() {
        return (false, ParseHealth::LogMissing);
    }
    let tail = match read_tail(&path) {
        Ok(t) => t,
        Err(_) => return (false, ParseHealth::ReadError),
    };
    let mut latest: Option<u64> = None;
    for line in tail.lines() {
        if let Some(idx) = line.find("Inhibit locks:") {
            let rest = line[idx + "Inhibit locks:".len()..].trim_start();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u64>() {
                latest = Some(n);
            }
        }
    }
    match latest {
        Some(n) => (n > 0, ParseHealth::Ok),
        None => (false, ParseHealth::NoMarkerFound),
    }
}

fn read_tail(path: &std::path::Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let size = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
