//! `profile list | current | switch <name>`.
//!
//! The daemon's MONITORS_CHANGED handler is the canonical apply path.
//! `switch` repoints .active.conf and runs `hyprctl reload`; the daemon
//! ingests the repoint (configreloaded RECONCILE, reconciler backstop) and
//! adopts the new edp policy. A manual switch is a force-apply, not a pin —
//! the next monitor-set change re-derives from the signature.

use crate::paths;
use crate::pure::profiles::match_in_signature;
use crate::sysio::profiles::{
    active_profile_name, load_profiles, monitor_signature, repoint_active_profile,
};

pub fn run(action: &str, name: Option<&str>) -> i32 {
    let profiles = load_profiles();
    let signature = monitor_signature();

    match action {
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
