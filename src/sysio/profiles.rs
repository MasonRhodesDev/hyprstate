//! io side of monitor profiles: directory loader (.conf + .lua dialects),
//! the .active.conf/.active.lua symlinks, and the hyprctl monitor signature
//! (sync — CLI use; the daemon gets async variants in its own module).

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::pure::profiles::ProfileFormat;

#[derive(Debug, Clone, PartialEq)]
pub struct TomlProfile {
    pub inner: monitor_profiles::Profile,
    pub format: ProfileFormat,
}

impl Deref for TomlProfile {
    type Target = monitor_profiles::Profile;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub fn config_dialect() -> ProfileFormat {
    #[cfg(test)]
    return ProfileFormat::Conf;

    #[cfg(not(test))]
    if paths::hyprland_lua_config().exists() {
        ProfileFormat::Lua
    } else {
        ProfileFormat::Conf
    }
}

/// Read every *.conf and *.lua in the profiles dir (excluding the
/// `.active.*` symlinks and any leading-dot file). When a stem exists in
/// both dialects (the migration window), the .lua profile wins. Malformed
/// profiles are logged to stderr and skipped; parse warnings are logged but
/// tolerated.
pub fn load_profiles() -> Vec<TomlProfile> {
    load_profiles_merged(&paths::profiles_dir(), paths::system_profiles_dir())
}

pub fn load_profiles_merged(user_dir: &Path, system_dir: &Path) -> Vec<TomlProfile> {
    let mut profiles = load_profiles_from(user_dir);

    // Profiles shared with the greeter live in the system directory. A
    // same-named user profile wins, so a per-user override never requires
    // editing /etc. Their Hyprland config is rendered into the *user*
    // directory: /etc is not ours to write, and the active-profile symlink
    // lives beside the user's profiles anyway.
    let (system, diagnostics) = load_toml_profiles_from(system_dir);
    for diagnostic in diagnostics {
        eprintln!("WARNING {}: {}", diagnostic.source, diagnostic.message);
    }
    for profile in system {
        if profiles.iter().any(|p| p.name == profile.name) {
            continue;
        }
        if let Err(e) = render_to_dir(user_dir, &profile) {
            eprintln!("WARNING rendering {}: {e}", profile.name);
        }
        profiles.push(profile);
    }
    profiles
}

pub fn load_toml_profiles_from(
    dir: &Path,
) -> (Vec<TomlProfile>, Vec<monitor_profiles::Diagnostic>) {
    let format = config_dialect();
    let (profiles, diagnostics) = monitor_profiles::load_dir(dir);
    (
        profiles
            .into_iter()
            .map(|inner| TomlProfile { inner, format })
            .collect(),
        diagnostics,
    )
}

fn format_of(path: &Path) -> Option<ProfileFormat> {
    match path.extension()?.to_str()? {
        "conf" => Some(ProfileFormat::Conf),
        "lua" => Some(ProfileFormat::Lua),
        _ => None,
    }
}

pub fn load_profiles_from(dir: &Path) -> Vec<TomlProfile> {
    let (toml_profiles, diagnostics) = load_toml_profiles_from(dir);
    for diagnostic in diagnostics {
        eprintln!("WARNING {}: {}", diagnostic.source, diagnostic.message);
    }
    if !toml_profiles.is_empty() {
        for profile in &toml_profiles {
            if let Err(e) = render_to_dir(dir, profile) {
                eprintln!("WARNING rendering {}: {e}", profile.name);
            }
        }
        return toml_profiles;
    }

    load_legacy_profiles_from(dir)
}

fn load_legacy_profiles_from(dir: &Path) -> Vec<TomlProfile> {
    let mut paths: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                format_of(p).is_some()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| !n.starts_with('.'))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    paths.sort();

    let mut profiles: Vec<TomlProfile> = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let format = format_of(&path).expect("filtered above");
        let Ok(text) = fs::read_to_string(&path) else {
            eprintln!("WARNING skipping unreadable profile {}", path.display());
            continue;
        };
        match monitor_profiles::legacy::to_profile(&name, &text) {
            Ok((inner, warnings)) => {
                for w in warnings {
                    eprintln!("WARNING {}: {w}", path.display());
                }
                // .conf sorts before .lua per stem, so a same-stem .lua
                // simply displaces its .conf twin here.
                let profile = TomlProfile { inner, format };
                if let Some(prev) = profiles
                    .iter_mut()
                    .find(|p| p.name == profile.name && p.format != profile.format)
                {
                    *prev = profile;
                } else {
                    profiles.push(profile);
                }
            }
            Err(e) => eprintln!("WARNING skipping malformed profile {}: {e}", path.display()),
        }
    }
    profiles
}

fn render_to_dir(dir: &Path, profile: &TomlProfile) -> std::io::Result<()> {
    let (content, warnings) = match profile.format {
        ProfileFormat::Conf => monitor_profiles::render::render_conf(profile),
        ProfileFormat::Lua => monitor_profiles::render::render_lua(profile),
    };
    for warning in warnings {
        eprintln!("WARNING {}: {warning}", profile.name);
    }
    write_if_changed_atomic(
        &dir.join(format!("{}.{}", profile.name, profile.format.ext())),
        &content,
    )
}

pub fn write_if_changed_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = PathBuf::from(path);
    let extension = path.extension().and_then(|x| x.to_str()).unwrap_or("");
    tmp.set_extension(format!("{extension}.tmp-{}", std::process::id()));
    fs::write(&tmp, content)?;
    fs::rename(tmp, path)
}

pub fn select_profile<'a>(
    signature: &[String],
    profiles: &'a [TomlProfile],
) -> Option<&'a TomlProfile> {
    let inner = profiles.iter().map(|p| p.inner.clone()).collect::<Vec<_>>();
    let selected = monitor_profiles::select(signature, &inner)?;
    profiles.iter().find(|p| p.name == selected.name)
}

/// Name (stem) of the profile the active symlink points at. `.active.lua`
/// wins over a (possibly stale) `.active.conf` during the migration window.
pub fn active_profile_name() -> Option<String> {
    [ProfileFormat::Lua, ProfileFormat::Conf]
        .into_iter()
        .find_map(|fmt| {
            let link = paths::active_profile_link(fmt);
            if !link.is_symlink() {
                return None;
            }
            fs::canonicalize(&link)
                .ok()?
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
}

/// Atomically repoint the active symlink of `target`'s dialect (tmp symlink,
/// then rename). When the profile also exists in the OTHER dialect, that
/// dialect's link is repointed too, so whichever config tree Hyprland is
/// currently reading (.conf sources `.active.conf`; hyprland.lua dofiles
/// `.active.lua`) always sees the switch — this is what keeps a manual or
/// daemon-driven profile change working mid-migration.
pub fn repoint_active_profile(target: &Path) -> std::io::Result<()> {
    let Some(format) = format_of(target) else {
        return Err(std::io::Error::other(format!(
            "profile target has no .conf/.lua extension: {}",
            target.display()
        )));
    };
    repoint_link(target, format)?;
    let twin_format = match format {
        ProfileFormat::Conf => ProfileFormat::Lua,
        ProfileFormat::Lua => ProfileFormat::Conf,
    };
    let twin = target.with_extension(twin_format.ext());
    if twin.exists() {
        repoint_link(&twin, twin_format)?;
    }
    Ok(())
}

fn repoint_link(target: &Path, format: ProfileFormat) -> std::io::Result<()> {
    let link = paths::active_profile_link(format);
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = link.with_extension(format!("{}.tmp", format.ext()));
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

    fn write_profile(dir: &Path, name: &str, output: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join(format!("{name}.toml")),
            format!(
                "match = [\"{output}\"]\n\n[[monitor]]\noutput = \"{output}\"\nmode = \"1920x1080@60\"\n"
            ),
        )
        .unwrap();
    }

    /// The system directory is what the greeter reads, so the session must
    /// see the same profiles from it -- and a same-named user profile must
    /// still win, so a per-user override never means editing /etc.
    #[test]
    fn system_profiles_merge_and_user_wins_on_name() {
        let base = std::env::temp_dir().join(format!("hyprstate-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (user, system) = (base.join("user"), base.join("system"));
        write_profile(&user, "desk", "DP-1");
        write_profile(&system, "desk", "HDMI-A-1");
        write_profile(&system, "shared-only", "DP-9");

        let profiles = load_profiles_merged(&user, &system);
        let mut names: Vec<_> = profiles.iter().map(|p| p.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["desk", "shared-only"]);

        let desk = profiles.iter().find(|p| p.name == "desk").unwrap();
        assert_eq!(
            desk.monitors[0].output, "DP-1",
            "the user's profile must win over the system one"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// An absent system directory is the normal case on a machine that ships
    /// no profiles: it must not warn, and must not disturb user profiles.
    #[test]
    fn absent_system_dir_is_harmless() {
        let base = std::env::temp_dir().join(format!("hyprstate-nosys-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let user = base.join("user");
        write_profile(&user, "solo", "DP-1");
        let profiles = load_profiles_merged(&user, &base.join("nonexistent"));
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "solo");
        let _ = fs::remove_dir_all(&base);
    }

    /// Port of test_hyprstate.py's load_profiles io test (deferred from M1).
    #[test]
    fn test_load_profiles_skips_dotfiles_and_missing_dir() {
        let dir = std::env::temp_dir().join(format!("hyprstate-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".active.conf"), "#@ match = A\n").unwrap();
        fs::write(dir.join(".active.lua"), "--@ match = A\n").unwrap();
        fs::write(dir.join("good.conf"), "#@ match = A\n").unwrap();
        fs::write(dir.join("bad.conf"), "monitor = no directives\n").unwrap();
        fs::write(dir.join("notes.txt"), "ignored\n").unwrap();
        let profiles = load_profiles_from(&dir);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "good");
        assert_eq!(profiles[0].format, ProfileFormat::Conf);
        assert!(load_profiles_from(&dir.join("nope")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Lua profiles load alongside .conf; a same-stem .lua displaces its
    /// .conf twin (migration window), and pure-Lua profiles parse `--@`.
    #[test]
    fn test_load_profiles_lua_dialect_and_collision() {
        let dir = std::env::temp_dir().join(format!("hyprstate-test-lua-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("twin.conf"), "#@ match = A\n#@ edp = enable\n").unwrap();
        fs::write(dir.join("twin.lua"), "--@ match = A\n--@ edp = disable\n").unwrap();
        fs::write(dir.join("solo.lua"), "--@ match = B\n").unwrap();
        let profiles = load_profiles_from(&dir);
        assert_eq!(profiles.len(), 2);
        let twin = profiles.iter().find(|p| p.name == "twin").unwrap();
        assert_eq!(twin.format, ProfileFormat::Lua);
        assert_eq!(twin.edp, crate::pure::profiles::EdpPolicy::Disable);
        let solo = profiles.iter().find(|p| p.name == "solo").unwrap();
        assert_eq!(solo.format, ProfileFormat::Lua);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toml_displaces_legacy_twin() {
        let dir = std::env::temp_dir().join(format!("hyprstate-test-toml-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.conf"), "#@ match = legacy\n").unwrap();
        fs::write(dir.join("a.toml"), "match = [\"toml\"]\n").unwrap();
        let profiles = load_profiles_from(&dir);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].matches, ["toml"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_toml_set_falls_back_to_legacy() {
        let dir =
            std::env::temp_dir().join(format!("hyprstate-test-fallback-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.conf"), "#@ match = legacy\n").unwrap();
        let profiles = load_profiles_from(&dir);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].matches, ["legacy"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rendered_file_carries_generated_header() {
        let dir =
            std::env::temp_dir().join(format!("hyprstate-test-header-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.toml"), "match = [\"A\"]\n").unwrap();
        let profiles = load_profiles_from(&dir);
        let rendered =
            fs::read_to_string(dir.join(format!("a.{}", profiles[0].format.ext()))).unwrap();
        assert!(rendered.contains("Do not edit"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
