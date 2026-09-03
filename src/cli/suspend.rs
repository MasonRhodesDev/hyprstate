//! `suspend request|cancel|status`: the idle-suspend request verb.
//!
//! Writes or removes `$XDG_RUNTIME_DIR/hyprstate-suspend-request`. This is a
//! REQUEST into the daemon's existing Countdown machinery, driven by
//! hypridle's idle timeout (request) and on-resume (cancel) — it never
//! suspends anything itself. The running daemon polls the file, enters
//! Countdown, proves a live locker, and only then calls logind Suspend.

use crate::paths;

pub fn run(action: &str) -> i32 {
    let path = paths::suspend_request_file();
    match action {
        "request" => match std::fs::write(&path, "idle\n") {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("hyprstate suspend request: {}: {e}", path.display());
                1
            }
        },
        "cancel" => match std::fs::remove_file(&path) {
            Ok(()) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => {
                eprintln!("hyprstate suspend cancel: {}: {e}", path.display());
                1
            }
        },
        "status" => {
            if path.exists() {
                println!("idle-suspend request: standing ({})", path.display());
            } else {
                println!("idle-suspend request: none");
            }
            0
        }
        other => {
            eprintln!("hyprstate suspend: unknown action {other:?}");
            2
        }
    }
}
