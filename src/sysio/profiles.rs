//! io side of monitor profiles: directory loader, the .active.conf symlink,
//! and the hyprctl monitor signature (sync — CLI use; the daemon gets async
//! variants in its own module).

use std::fs;
use std::path::Path;

use crate::paths;
use crate::pure::profiles::{Profile, parse_profile};

/// Read every *.conf in the profiles dir (excluding the `.active.conf`
/// symlink and any leading-dot file). Malformed profiles are logged to
/// stderr and skipped; parse warnings are logged but tolerated.
pub fn load_profiles() -> Vec<Profile> {
    load_profiles_from(&paths::profiles_dir())
}

pub fn load_profiles_from(dir: &Path) -> Vec<Profile> {
    let mut paths: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "conf")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| !n.starts_with('.'))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    paths.sort();

    let mut profiles = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(text) = fs::read_to_string(&path) else {
            eprintln!("WARNING skipping unreadable profile {}", path.display());
            continue;
        };
        match parse_profile(&name, &text) {
            Ok((profile, warnings)) => {
                for w in warnings {
                    eprintln!("WARNING {}: {w}", path.display());
                }
                profiles.push(profile);
            }
            Err(e) => eprintln!("WARNING skipping malformed profile {}: {e}", path.display()),
        }
    }
    profiles
}

/// Name (stem) of the profile `.active.conf` currently points at.
pub fn active_profile_name() -> Option<String> {
    let link = paths::active_profile_link();
    if !link.is_symlink() {
        return None;
    }
    fs::canonicalize(&link)
        .ok()?
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
}

/// Atomically repoint .active.conf at `target` (tmp symlink + rename).
pub fn repoint_active_profile(target: &Path) -> std::io::Result<()> {
    let link = paths::active_profile_link();
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = link.with_extension("conf.tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, &link)
}

/// All monitors (including disabled) from `hyprctl monitors all -j`, for
/// `profile save` capture. Empty on any failure.
pub fn monitor_snapshot_all() -> Vec<crate::pure::profiles::MonitorSnapshot> {
    let out = match std::process::Command::new("hyprctl")
        .args(["monitors", "all", "-j"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("WARNING hyprctl monitors all failed: {e}");
            return Vec::new();
        }
    };
    let Ok(monitors) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        eprintln!("WARNING monitor_snapshot_all: bad hyprctl json");
        return Vec::new();
    };
    monitors
        .iter()
        .map(|m| {
            let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let n = |k: &str| m.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            crate::pure::profiles::MonitorSnapshot {
                name: s("name"),
                description: s("description"),
                width: n("width") as u32,
                height: n("height") as u32,
                refresh: n("refreshRate"),
                x: n("x") as i32,
                y: n("y") as i32,
                scale: n("scale"),
                transform: n("transform") as u8,
                disabled: m.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
            }
        })
        .collect()
}

/// Snapshot of currently-connected monitor descriptions from hyprctl.
pub fn monitor_signature() -> Vec<String> {
    let out = match std::process::Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("WARNING monitor_signature failed: {e}");
            return Vec::new();
        }
    };
    let Ok(monitors) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        eprintln!("WARNING monitor_signature: bad hyprctl json");
        return Vec::new();
    };
    monitors
        .iter()
        .map(|m| {
            m.get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of test_hyprstate.py's load_profiles io test (deferred from M1).
    #[test]
    fn test_load_profiles_skips_dotfiles_and_missing_dir() {
        let dir = std::env::temp_dir().join(format!("hyprstate-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".active.conf"), "#@ match = A\n").unwrap();
        fs::write(dir.join("good.conf"), "#@ match = A\n").unwrap();
        fs::write(dir.join("bad.conf"), "monitor = no directives\n").unwrap();
        fs::write(dir.join("notes.txt"), "ignored\n").unwrap();
        let profiles = load_profiles_from(&dir);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "good");
        assert!(load_profiles_from(&dir.join("nope")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
