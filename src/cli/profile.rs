//! `profile list | current | switch <name> | save <name>`.
//!
//! The daemon's MONITORS_CHANGED handler is the canonical apply path.
//! `switch` repoints .active.conf and runs `hyprctl reload`; the daemon
//! ingests the repoint (configreloaded RECONCILE, reconciler backstop) and
//! adopts the new edp policy. A manual switch is a force-apply, not a pin —
//! the next monitor-set change re-derives from the signature.
//!
//! `save` captures the LIVE monitor layout as a new profile (the editor
//! workflow folded in from the archived hyprdm: arrange monitors with
//! whatever tool you like, then snapshot the result).

use crate::paths;
use crate::pure::profiles::{EdpPolicy, GpuPref, match_in_signature, render_profile};
use crate::sysio::profiles::{
    active_profile_name, load_profiles, monitor_signature, monitor_snapshot_all,
    repoint_active_profile,
};

pub struct SaveOpts {
    pub edp: EdpPolicy,
    pub gpu: GpuPref,
    pub priority: Option<i64>,
    pub force: bool,
}

pub fn run(action: &str, name: Option<&str>, save: &SaveOpts) -> i32 {
    let profiles = load_profiles();
    let signature = monitor_signature();

    match action {
        "save" => {
            let Some(name) = name else {
                eprintln!("save requires a profile name");
                return 2;
            };
            let valid = !name.is_empty()
                && !name.starts_with('.')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if !valid {
                eprintln!("profile names are [A-Za-z0-9._-]+ and must not start with '.'");
                return 2;
            }
            let target = paths::profiles_dir().join(format!("{name}.conf"));
            if target.exists() && !save.force {
                eprintln!("profile {name} already exists — use --force to overwrite");
                return 1;
            }
            let monitors = monitor_snapshot_all();
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let (text, warnings) =
                match render_profile(name, &date, &monitors, save.edp, save.gpu, save.priority) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("capture failed: {e}");
                        return 1;
                    }
                };
            for w in warnings {
                eprintln!("WARNING {w}");
            }
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&target, &text) {
                eprintln!("write failed: {e}");
                return 1;
            }
            println!("saved {}", target.display());
            for line in text.lines().filter(|l| l.starts_with("#@ match")) {
                println!("  {line}");
            }
            println!(
                "the daemon auto-selects by signature on the next monitor change; \
                 `hyprstate profile switch {name}` applies it now"
            );
            0
        }
        "list" => {
            let mut sorted = profiles;
            // Stable: priority descending, load (filename) order for ties.
            sorted.sort_by_key(|p| std::cmp::Reverse(p.priority));
            for p in &sorted {
                let applies = if p.matches.iter().all(|m| match_in_signature(m, &signature)) {
                    "✓"
                } else {
                    " "
                };
                println!(
                    "  [{applies}] {:<28} prio={} edp={:<7} match=[{}]",
                    p.name,
                    p.priority,
                    p.edp.as_str(),
                    p.matches.join(", ")
                );
            }
            0
        }
        "current" => {
            match active_profile_name() {
                Some(name) => println!("{name}"),
                None => println!("(no active profile)"),
            }
            0
        }
        "switch" => {
            let Some(name) = name else {
                eprintln!("switch requires a profile name");
                return 2;
            };
            let Some(profile) = profiles.iter().find(|p| p.name == name) else {
                eprintln!("unknown profile: {name}");
                eprintln!(
                    "available: {}",
                    profiles
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return 1;
            };
            let target = paths::profiles_dir().join(format!("{}.conf", profile.name));
            if let Err(e) = repoint_active_profile(&target) {
                eprintln!("symlink failed: {e}");
                return 1;
            }
            let _ = std::process::Command::new("hyprctl").arg("reload").status();
            println!("switched to {name}");
            0
        }
        other => {
            eprintln!("unknown action: {other}");
            2
        }
    }
}
