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
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use crate::pure::profiles::{
    EdpPolicy, GpuPref, ProfileFormat, render_profile, render_profile_lua,
};
use crate::sysio::profiles::{
    active_profile_name, load_profiles, monitor_signature, monitor_snapshot_all,
    repoint_active_profile, write_if_changed_atomic,
};

pub struct SaveOpts {
    pub edp: EdpPolicy,
    pub gpu: GpuPref,
    pub priority: Option<i64>,
    pub force: bool,
    pub dry_run: bool,
    /// None = auto: Lua iff ~/.config/hypr/hyprland.lua exists (i.e. the
    /// machine's Hyprland config has migrated).
    pub format: Option<ProfileFormat>,
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn format_number(value: f64) -> String {
    let formatted = format!("{value:.2}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

pub fn to_toml(profile: &monitor_profiles::Profile) -> String {
    let mut out = String::new();
    if !profile.description.is_empty() {
        out.push_str(&format!("description = {}\n", quote(&profile.description)));
    }
    out.push_str("match = [");
    out.push_str(
        &profile
            .matches
            .iter()
            .map(|x| quote(x))
            .collect::<Vec<_>>()
            .join(", "),
    );
    out.push_str("]\n");
    if profile.edp != EdpPolicy::Auto {
        out.push_str(&format!("edp = {:?}\n", profile.edp.as_str()));
    }
    if profile.gpu != GpuPref::Auto {
        out.push_str(&format!("gpu = {:?}\n", profile.gpu.as_str()));
    }
    if !profile.hooks.is_empty() {
        out.push_str("hooks = [");
        out.push_str(
            &profile
                .hooks
                .iter()
                .map(|x| quote(x))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str("]\n");
    }
    if profile.priority != profile.matches.len() as i64 {
        out.push_str(&format!("priority = {}\n", profile.priority));
    }
    for monitor in &profile.monitors {
        out.push_str("\n[[monitor]]\n");
        out.push_str(&format!("output = {}\n", quote(&monitor.output)));
        if let Some(mode) = monitor.mode {
            out.push_str(&format!("mode = {}\n", quote(&mode.to_string())));
        }
        if monitor.scale != 1.0 {
            out.push_str(&format!("scale = {}\n", format_number(monitor.scale)));
        }
        if let Some((x, y)) = monitor.position {
            out.push_str(&format!("position = [{x}, {y}]\n"));
        }
        if monitor.transform != 0 {
            out.push_str(&format!("transform = {}\n", monitor.transform));
        }
        if !monitor.enabled {
            out.push_str("enabled = false\n");
        }
    }
    for workspace in &profile.workspaces {
        out.push_str("\n[[workspace]]\n");
        out.push_str(&format!("workspace = {}\n", quote(&workspace.workspace)));
        out.push_str(&format!("monitor = {}\n", quote(&workspace.monitor)));
        if workspace.default {
            out.push_str("default = true\n");
        }
    }
    out
}

fn difference<T: PartialEq + Debug>(
    differences: &mut Vec<String>,
    label: &str,
    field: &str,
    legacy: &T,
    toml: &T,
) {
    if legacy != toml {
        differences.push(format!("{label}: {field} legacy={legacy:?} toml={toml:?}"));
    }
}

pub fn verify_profiles(
    legacy: &monitor_profiles::Profile,
    toml: &monitor_profiles::Profile,
    connected: Option<&[monitor_profiles::ConnectedOutput]>,
) -> Vec<String> {
    let mut differences = Vec::new();
    let label = legacy.name.as_str();
    difference(
        &mut differences,
        label,
        "matches",
        &legacy.matches,
        &toml.matches,
    );
    difference(&mut differences, label, "edp", &legacy.edp, &toml.edp);
    difference(&mut differences, label, "gpu", &legacy.gpu, &toml.gpu);
    difference(
        &mut differences,
        label,
        "priority",
        &legacy.priority,
        &toml.priority,
    );
    difference(&mut differences, label, "hooks", &legacy.hooks, &toml.hooks);
    difference(
        &mut differences,
        label,
        "workspaces",
        &legacy.workspaces,
        &toml.workspaces,
    );
    compare_layouts(
        &mut differences,
        label,
        &monitor_profiles::resolve_all(legacy),
        &monitor_profiles::resolve_all(toml),
    );
    if let Some(connected) = connected {
        compare_layouts(
            &mut differences,
            label,
            &monitor_profiles::resolve(legacy, connected),
            &monitor_profiles::resolve(toml, connected),
        );
    }
    differences
}

fn compare_layouts(
    differences: &mut Vec<String>,
    profile_name: &str,
    legacy: &monitor_profiles::ResolvedLayout,
    toml: &monitor_profiles::ResolvedLayout,
) {
    difference(
        differences,
        profile_name,
        "output-count",
        &legacy.outputs.len(),
        &toml.outputs.len(),
    );
    for (a, b) in legacy.outputs.iter().zip(&toml.outputs) {
        let label = format!("{profile_name}.{}", a.selector);
        difference(differences, &label, "selector", &a.selector, &b.selector);
        difference(differences, &label, "mode", &a.mode, &b.mode);
        difference(differences, &label, "position", &a.position, &b.position);
        difference(differences, &label, "scale", &a.scale, &b.scale);
        difference(differences, &label, "transform", &a.transform, &b.transform);
        difference(differences, &label, "enabled", &a.enabled, &b.enabled);
    }
}

fn legacy_files(dir: &Path, only: Option<&str>) -> Vec<PathBuf> {
    let mut files = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let extension = path.extension().and_then(|x| x.to_str());
            let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
            matches!(extension, Some("conf" | "lua"))
                && !name.starts_with('.')
                && !name.starts_with(".active.")
                && only.is_none_or(|wanted| path.file_stem().is_some_and(|x| x == wanted))
        })
        .collect::<Vec<_>>();
    files.sort();
    // Match legacy loading: a same-stem Lua file displaces Conf. `dedup_by`
    // passes the *later* element as `a` and drops it when the closure returns
    // true, so the surviving element is `b` — overwrite `b` with the Lua path,
    // not the other way around. (Getting this backwards made `migrate` convert
    // the stub `.conf` twin and emit a profile with no monitors at all.)
    files.dedup_by(|a, b| {
        if a.file_stem() == b.file_stem() {
            if a.extension().and_then(|x| x.to_str()) == Some("lua") {
                *b = a.clone();
            }
            true
        } else {
            false
        }
    });
    files
}

fn verify_dir(dir: &Path, only: Option<&str>) -> (usize, Vec<String>) {
    let snapshots = monitor_snapshot_all();
    let connected = snapshots
        .iter()
        .map(|m| monitor_profiles::ConnectedOutput {
            name: m.name.clone(),
            description: m.description.clone(),
        })
        .collect::<Vec<_>>();
    let signature = monitor_signature();
    let mut count = 0;
    let mut differences = Vec::new();
    for legacy_path in legacy_files(dir, only) {
        let name = legacy_path.file_stem().unwrap().to_string_lossy();
        let toml_path = legacy_path.with_extension("toml");
        if !toml_path.exists() {
            continue;
        }
        let Ok(legacy_text) = fs::read_to_string(&legacy_path) else {
            differences.push(format!("{name}: cannot read legacy file"));
            continue;
        };
        let Ok(toml_text) = fs::read_to_string(&toml_path) else {
            differences.push(format!("{name}: cannot read TOML file"));
            continue;
        };
        let Ok((a, _)) = monitor_profiles::legacy::to_profile(&name, &legacy_text) else {
            differences.push(format!("{name}: cannot parse legacy file"));
            continue;
        };
        let Ok((b, _)) = monitor_profiles::from_toml(&name, &toml_text) else {
            differences.push(format!("{name}: cannot parse TOML file"));
            continue;
        };
        let live = b
            .matches
            .iter()
            .all(|m| monitor_profiles::match_in_signature(m, &signature))
            .then_some(connected.as_slice());
        if a.monitors.is_empty() && b.monitors.is_empty() {
            differences.push(format!(
                "{name}: resolves to zero monitors on both sides — the layout was \
                 not preserved (an empty-vs-empty comparison is not a match)"
            ));
            continue;
        }
        differences.extend(verify_profiles(&a, &b, live));
        count += 1;
    }
    (count, differences)
}

pub fn run(action: &str, name: Option<&str>, save: &SaveOpts) -> i32 {
    // migrate/verify are pure file operations. Loading the profile set and
    // querying the compositor for them re-parses every legacy file (echoing
    // migrate's own warnings) and fails noisily off-session.
    let needs_live = matches!(action, "list" | "current" | "switch" | "save");
    let profiles = if needs_live {
        load_profiles()
    } else {
        Vec::new()
    };
    let signature = if needs_live {
        monitor_signature()
    } else {
        Vec::new()
    };

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
            let format = save.format.unwrap_or_else(|| {
                if paths::hyprland_lua_config().exists() {
                    ProfileFormat::Lua
                } else {
                    ProfileFormat::Conf
                }
            });
            let target = paths::profiles_dir().join(format!("{name}.toml"));
            if target.exists() && !save.force {
                eprintln!("profile {name} already exists — use --force to overwrite");
                return 1;
            }
            let monitors = monitor_snapshot_all();
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let render = match format {
                ProfileFormat::Conf => render_profile,
                ProfileFormat::Lua => render_profile_lua,
            };
            let (text, warnings) =
                match render(name, &date, &monitors, save.edp, save.gpu, save.priority) {
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
            let (profile, migration_warnings) =
                match monitor_profiles::legacy::to_profile(name, &text) {
                    Ok(result) => result,
                    Err(e) => {
                        eprintln!("captured profile could not be converted: {e}");
                        return 1;
                    }
                };
            for warning in migration_warnings {
                eprintln!("WARNING {warning}");
            }
            let toml = to_toml(&profile);
            if let Err(e) = write_if_changed_atomic(&target, &toml) {
                eprintln!("write failed: {e}");
                return 1;
            }
            let rendered_target = target.with_extension(format.ext());
            let (rendered, render_warnings) = match format {
                ProfileFormat::Conf => monitor_profiles::render::render_conf(&profile),
                ProfileFormat::Lua => monitor_profiles::render::render_lua(&profile),
            };
            for warning in render_warnings {
                eprintln!("WARNING {warning}");
            }
            if let Err(e) = write_if_changed_atomic(&rendered_target, &rendered) {
                eprintln!("render write failed: {e}");
                return 1;
            }
            println!("saved {}", target.display());
            for matched in &profile.matches {
                println!("  match = {matched}");
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
                let applies = if p
                    .matches
                    .iter()
                    .all(|m| monitor_profiles::match_in_signature(m, &signature))
                {
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
            let target =
                paths::profiles_dir().join(format!("{}.{}", profile.name, profile.format.ext()));
            if let Err(e) = repoint_active_profile(&target) {
                eprintln!("symlink failed: {e}");
                return 1;
            }
            let _ = std::process::Command::new("hyprctl").arg("reload").status();
            println!("switched to {name}");
            0
        }
        "migrate" => {
            let dir = paths::profiles_dir();
            let mut converted = 0;
            for legacy_path in legacy_files(&dir, name) {
                let stem = legacy_path.file_stem().unwrap().to_string_lossy();
                let target = legacy_path.with_extension("toml");
                if target.exists() && !save.force {
                    println!("skipping {} (TOML exists)", target.display());
                    continue;
                }
                let text = match fs::read_to_string(&legacy_path) {
                    Ok(text) => text,
                    Err(e) => {
                        eprintln!("{}: {e}", legacy_path.display());
                        return 1;
                    }
                };
                let (profile, warnings) = match monitor_profiles::legacy::to_profile(&stem, &text) {
                    Ok(result) => result,
                    Err(e) => {
                        eprintln!("{}: {e}", legacy_path.display());
                        return 1;
                    }
                };
                for warning in warnings {
                    eprintln!("WARNING {}: {warning}", legacy_path.display());
                }
                // A conversion that yields no monitors has silently thrown the
                // layout away. Refuse it: verify compares parse-to-parse and
                // would call empty-vs-empty "identical", which is precisely the
                // false confidence this gate exists to prevent.
                if profile.monitors.is_empty() {
                    eprintln!(
                        "{}: converts to zero monitors — refusing to write {}. \
                         Convert this profile by hand.",
                        legacy_path.display(),
                        target.display()
                    );
                    return 1;
                }
                let serialized = to_toml(&profile);
                if save.dry_run {
                    println!("would write {}", target.display());
                } else if let Err(e) = write_if_changed_atomic(&target, &serialized) {
                    eprintln!("{}: {e}", target.display());
                    return 1;
                } else {
                    println!("wrote {}", target.display());
                }
                converted += 1;
            }
            if save.dry_run {
                println!("{converted} profiles would be migrated");
                return 0;
            }
            let (count, differences) = verify_dir(&dir, name);
            if !differences.is_empty() {
                for difference in differences {
                    eprintln!("{difference}");
                }
                return 1;
            }
            println!("{count} profiles verified identical");
            0
        }
        "verify" => {
            let (count, differences) = verify_dir(&paths::profiles_dir(), name);
            if differences.is_empty() {
                println!("{count} profiles verified identical");
                0
            } else {
                for difference in differences {
                    eprintln!("{difference}");
                }
                1
            }
        }
        other => {
            eprintln!("unknown action: {other}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitor_profiles::{Mode, Monitor, Profile};

    /// A stub `.conf` next to a real `.lua` must not be what migrate reads:
    /// the Lua twin is what the session actually loads. Getting this backwards
    /// converted the stub and produced a TOML with no monitors in it.
    #[test]
    fn lua_twin_displaces_conf_for_migration() {
        // No tempfile dev-dependency: it would enter the vendored tarball
        // the RPM %check builds from.
        let dir = std::env::temp_dir().join(format!("hyprstate-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in ["desk.conf", "desk.lua", "solo.conf"] {
            fs::write(dir.join(name), "").unwrap();
        }
        let picked = legacy_files(&dir, None);
        let names = picked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(names, vec!["desk.lua", "solo.conf"]);
    }

    fn profile(monitors: Vec<Monitor>) -> Profile {
        Profile {
            name: "test".into(),
            description: String::new(),
            matches: vec!["Panel".into()],
            edp: EdpPolicy::Auto,
            gpu: GpuPref::Auto,
            hooks: vec![],
            priority: 1,
            monitors,
            workspaces: vec![],
        }
    }

    fn monitor(output: &str, scale: f64, position: Option<(i32, i32)>) -> Monitor {
        Monitor {
            output: output.into(),
            mode: Mode::parse("3840x2160@60"),
            scale,
            position,
            transform: 0,
            enabled: true,
        }
    }

    #[test]
    fn verify_detects_scale_drift() {
        let a = profile(vec![monitor("DP-1", 1.5, Some((0, 0)))]);
        let b = profile(vec![monitor("DP-1", 1.6, Some((0, 0)))]);
        let differences = verify_profiles(&a, &b, None);
        assert_eq!(differences.len(), 1);
        assert!(differences[0].contains(": scale "));
    }

    #[test]
    fn verify_accepts_identical() {
        let a = profile(vec![monitor("DP-1", 1.5, Some((0, 0)))]);
        assert!(verify_profiles(&a, &a, None).is_empty());
    }

    #[test]
    fn verify_catches_position_source_difference() {
        let a = profile(vec![
            monitor("DP-1", 1.5, Some((0, 0))),
            monitor("DP-2", 1.5, Some((0, 0))),
        ]);
        let b = profile(vec![monitor("DP-1", 1.5, None), monitor("DP-2", 1.5, None)]);
        let differences = verify_profiles(&a, &b, None);
        assert!(differences.iter().any(|d| d.contains("position")));
    }

    fn round_trip(text: &str, name: &str) -> (Profile, Profile) {
        let (legacy, warnings) = monitor_profiles::legacy::to_profile(name, text).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let serialized = to_toml(&legacy);
        let (toml, warnings) = monitor_profiles::from_toml(name, &serialized).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(verify_profiles(&legacy, &toml, None).is_empty());
        (legacy, toml)
    }

    #[test]
    fn round_trip_laptop_only() {
        let (_, profile) = round_trip(
            include_str!("../../tests/fixtures/laptop-only.conf"),
            "laptop-only",
        );
        let output = &monitor_profiles::resolve_all(&profile).outputs[0];
        assert_eq!(output.selector, "eDP-2");
        assert_eq!(output.position, (0, 0));
        assert_eq!(output.scale, 1.25);
    }

    #[test]
    fn round_trip_dual_4k() {
        let (_, profile) = round_trip(include_str!("../../tests/fixtures/dual-4k.lua"), "dual-4k");
        let layout = monitor_profiles::resolve_all(&profile);
        assert_eq!(
            layout
                .outputs
                .iter()
                .map(|o| o.position.0)
                .collect::<Vec<_>>(),
            vec![0, 2560, 5120]
        );
        assert_eq!(
            layout.outputs.iter().map(|o| o.scale).collect::<Vec<_>>(),
            vec![1.5, 1.5, 1.6]
        );
    }
}
