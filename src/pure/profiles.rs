//! Monitor-profile parsing and selection.
//!
//! A profile is a Hyprland .conf snippet with `#@ key = value` directive
//! comments in its leading comment block. Parsing here is pure over `&str`;
//! the io layer globs the profiles dir, feeds file contents in, and logs the
//! returned warnings.

/// `#@ edp = ...` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdpPolicy {
    #[default]
    Auto,
    Enable,
    Disable,
}

impl EdpPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            EdpPolicy::Auto => "auto",
            EdpPolicy::Enable => "enable",
            EdpPolicy::Disable => "disable",
        }
    }
}

/// `#@ gpu = ...` render-GPU preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuPref {
    #[default]
    Auto,
    Igpu,
    Dgpu,
}

impl GpuPref {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuPref::Auto => "auto",
            GpuPref::Igpu => "igpu",
            GpuPref::Dgpu => "dgpu",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub matches: Vec<String>,
    pub edp: EdpPolicy,
    pub gpu: GpuPref,
    pub hooks: Vec<String>,
    /// Explicit `#@ priority`; defaults to matches.len().
    pub priority: i64,
}

/// Parse one `#@ key = value` line. Mirrors v1's directive regexes:
/// profile keys are `[a-z]+` (NO hyphens — "battery-low" must not be a legal
/// monitor-profile key); power.conf keys are `[a-z][a-z-]*`.
pub fn parse_directive(line: &str, allow_hyphen: bool) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("#@")?.trim_start();
    let eq = rest.find('=')?;
    let key = rest[..eq].trim_end();
    let val = rest[eq + 1..].trim();
    if key.is_empty() || val.is_empty() {
        return None;
    }
    let key_ok = key
        .chars()
        .enumerate()
        .all(|(i, c)| c.is_ascii_lowercase() || (allow_hyphen && i > 0 && c == '-'));
    key_ok.then_some((key, val))
}

/// Parse a profile body. `Err` = profile is malformed and must be skipped
/// (no match directives, invalid edp/gpu/priority value). Warnings cover
/// malformed/unknown directives that v1 logged but tolerated.
pub fn parse_profile(name: &str, text: &str) -> Result<(Profile, Vec<String>), String> {
    let mut matches: Vec<String> = Vec::new();
    let mut hooks: Vec<String> = Vec::new();
    let mut edp = EdpPolicy::Auto;
    let mut gpu = GpuPref::Auto;
    let mut priority: Option<i64> = None;
    let mut warnings: Vec<String> = Vec::new();

    for line in text.lines() {
        if !line.starts_with("#@") {
            // Stop scanning once the body begins. Directives must all sit in
            // the leading comment block — anything below passes through to
            // Hyprland as-is.
            if line.trim_start().starts_with('#') || line.trim().is_empty() {
                continue;
            }
            break;
        }
        let Some((key, val)) = parse_directive(line, false) else {
            warnings.push(format!("ignoring malformed directive: {line:?}"));
            continue;
        };
        match key {
            "match" => matches.push(val.to_string()),
            "hook" => hooks.push(val.to_string()),
            "edp" => {
                edp = match val {
                    "auto" => EdpPolicy::Auto,
                    "enable" => EdpPolicy::Enable,
                    "disable" => EdpPolicy::Disable,
                    _ => return Err(format!("edp must be auto|enable|disable, got {val:?}")),
                }
            }
            "gpu" => {
                gpu = match val {
                    "auto" => GpuPref::Auto,
                    "igpu" => GpuPref::Igpu,
                    "dgpu" => GpuPref::Dgpu,
                    _ => return Err(format!("gpu must be auto|igpu|dgpu, got {val:?}")),
                }
            }
            "priority" => {
                priority = Some(
                    val.parse()
                        .map_err(|_| format!("priority must be an integer, got {val:?}"))?,
                )
            }
            other => warnings.push(format!("unknown directive {other:?}")),
        }
    }

    if matches.is_empty() {
        return Err("profile has no `#@ match = ...` directives".into());
    }
    Ok((
        Profile {
            name: name.to_string(),
            priority: priority.unwrap_or(matches.len() as i64),
            matches,
            edp,
            gpu,
            hooks,
        },
        warnings,
    ))
}

/// A `#@ match = ...` directive matches if any detected monitor description
/// starts with the directive's value. The `desc:` prefix (Hyprland syntax)
/// is stripped so users can paste rules from monitors.conf verbatim.
pub fn match_in_signature(m: &str, signature: &[String]) -> bool {
    let needle = m.strip_prefix("desc:").unwrap_or(m).trim();
    signature.iter().any(|desc| desc.starts_with(needle))
}

/// Pure: pick the profile whose match set is a subset of `signature`,
/// breaking ties by (priority, match count, name) — all descending via max.
pub fn select_profile<'a>(signature: &[String], profiles: &'a [Profile]) -> Option<&'a Profile> {
    profiles
        .iter()
        .filter(|p| p.matches.iter().all(|m| match_in_signature(m, signature)))
        .max_by(|a, b| {
            (a.priority, a.matches.len(), &a.name).cmp(&(b.priority, b.matches.len(), &b.name))
        })
}

// =========================================================================
// Profile capture (`profile save` — the editor folded in from hyprdm)
// =========================================================================

/// One monitor from `hyprctl monitors all -j`, as capture needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorSnapshot {
    pub name: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub refresh: f64,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u8,
    pub disabled: bool,
}

/// "165.00000" -> "165", "1.25" -> "1.25" — matches hand-written profiles.
fn fmt_num(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Hyprland monitor selector. Convention from the hand-written profiles:
/// the internal panel is addressed by NAME (eDP-*), externals by stable
/// `desc:`. Descriptions containing commas would shatter the monitor
/// keyword's comma-separated syntax — fall back to the (unstable) name and
/// warn.
fn selector(m: &MonitorSnapshot, warnings: &mut Vec<String>) -> String {
    if m.name.starts_with("eDP") {
        return m.name.clone();
    }
    if m.description.contains(',') {
        warnings.push(format!(
            "{}: description contains a comma — using the connector name \
             (NOT stable across replugs): {:?}",
            m.name, m.description
        ));
        return m.name.clone();
    }
    format!("desc:{}", m.description)
}

/// Render the current layout as a profile body (the capture side of
/// `profile save`). Conventions, all lifted from the hand-written profiles:
/// - `#@ match` on EXTERNAL descriptions only, so the profile keeps matching
///   when the lid closes and eDP leaves the signature; a layout with no
///   externals matches on the eDP description (the laptop-only case).
/// - Enabled monitors sorted left-to-right; disabled ones pinned `disable`.
/// - Default priority (match count) is left implicit unless overridden.
pub fn render_profile(
    name: &str,
    date: &str,
    monitors: &[MonitorSnapshot],
    edp: EdpPolicy,
    gpu: GpuPref,
    priority: Option<i64>,
) -> Result<(String, Vec<String>), String> {
    let mut enabled: Vec<&MonitorSnapshot> = monitors.iter().filter(|m| !m.disabled).collect();
    if enabled.is_empty() {
        return Err("no enabled monitors to capture".into());
    }
    enabled.sort_by_key(|m| (m.x, m.y));
    let disabled: Vec<&MonitorSnapshot> = monitors.iter().filter(|m| m.disabled).collect();

    let mut warnings = Vec::new();
    let externals: Vec<&&MonitorSnapshot> = enabled
        .iter()
        .filter(|m| !m.name.starts_with("eDP"))
        .collect();
    let match_descs: Vec<&str> = if externals.is_empty() {
        enabled.iter().map(|m| m.description.as_str()).collect()
    } else {
        externals.iter().map(|m| m.description.as_str()).collect()
    };

    let mut out = String::new();
    out.push_str(&format!(
        "# Profile: {name} — captured from the live layout ({date}).\n#\n"
    ));
    for desc in &match_descs {
        out.push_str(&format!("#@ match = desc:{desc}\n"));
    }
    out.push_str(&format!("#@ edp = {}\n", edp.as_str()));
    if gpu != GpuPref::Auto {
        out.push_str(&format!("#@ gpu = {}\n", gpu.as_str()));
    }
    if let Some(p) = priority {
        out.push_str(&format!("#@ priority = {p}\n"));
    }
    out.push('\n');

    for m in &enabled {
        let mut line = format!(
            "monitor = {},{}x{}@{},{}x{},{}",
            selector(m, &mut warnings),
            m.width,
            m.height,
            fmt_num(m.refresh),
            m.x,
            m.y,
            fmt_num(m.scale)
        );
        if m.transform != 0 {
            line.push_str(&format!(",transform,{}", m.transform));
        }
        out.push_str(&line);
        out.push('\n');
    }
    for m in &disabled {
        out.push_str(&format!(
            "monitor = {},disable\n",
            selector(m, &mut warnings)
        ));
    }

    out.push_str(
        "\n# Add workspace pinning if desired, e.g.:\n\
         # workspace = 1, monitor:desc:..., default:true\n",
    );
    Ok((out, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str, text: &str) -> Result<(Profile, Vec<String>), String> {
        parse_profile(name, text)
    }

    #[test]
    fn test_parse_profile_full() {
        let (prof, warnings) = parse(
            "desk",
            "#@ match = desc:Dell U2723QE\n\
             #@ match = desc:LG HDR 4K\n\
             #@ edp = disable\n\
             #@ gpu = dgpu\n\
             #@ hook = notify-send applied\n\
             #@ priority = 10\n\
             monitor = desc:Dell U2723QE, 3840x2160@60, 0x0, 1.5\n",
        )
        .unwrap();
        assert_eq!(prof.name, "desk");
        assert_eq!(prof.matches, vec!["desc:Dell U2723QE", "desc:LG HDR 4K"]);
        assert_eq!(prof.edp, EdpPolicy::Disable);
        assert_eq!(prof.gpu, GpuPref::Dgpu);
        assert_eq!(prof.hooks, vec!["notify-send applied"]);
        assert_eq!(prof.priority, 10);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_parse_profile_default_priority_is_match_count() {
        let (prof, _) = parse("two", "#@ match = A\n#@ match = B\n").unwrap();
        assert_eq!(prof.priority, 2);
    }

    #[test]
    fn test_parse_profile_directives_stop_at_body() {
        let (prof, _) = parse(
            "body",
            "#@ match = A\nmonitor = something\n#@ edp = disable\n",
        )
        .unwrap();
        assert_eq!(prof.edp, EdpPolicy::Auto); // post-body directive ignored
    }

    #[test]
    fn test_malformed_profiles_are_rejected() {
        // No match directives at all.
        assert!(parse("bad", "monitor = no directives at all\n").is_err());
        // Invalid edp value.
        assert!(parse("bad", "#@ match = A\n#@ edp = sideways\n").is_err());
        // Invalid gpu value.
        assert!(parse("bad", "#@ match = A\n#@ gpu = both\n").is_err());
        // Non-integer priority.
        assert!(parse("bad", "#@ match = A\n#@ priority = high\n").is_err());
    }

    #[test]
    fn test_malformed_and_unknown_directives_warn_but_tolerate() {
        let (prof, warnings) =
            parse("tolerant", "#@ match = A\n#@ !!bad line\n#@ mystery = 7\n").unwrap();
        assert_eq!(prof.matches, vec!["A"]);
        assert_eq!(warnings.len(), 2);
    }

    /// Hyphenated keys are only legal with allow_hyphen (the power.conf
    /// dialect) — "battery-low" must not parse as a profile directive key.
    #[test]
    fn test_directive_key_charsets() {
        assert!(parse_directive("#@ battery-low = x", false).is_none());
        assert_eq!(
            parse_directive("#@ battery-low = x", true),
            Some(("battery-low", "x"))
        );
        assert_eq!(parse_directive("#@match=x", false), Some(("match", "x")));
        assert!(parse_directive("#@ match = ", false).is_none());
        assert!(parse_directive("#@ Match = x", false).is_none());
    }

    fn prof(name: &str, matches: &[&str], priority: Option<i64>) -> Profile {
        Profile {
            name: name.to_string(),
            priority: priority.unwrap_or(matches.len() as i64),
            matches: matches.iter().map(|s| s.to_string()).collect(),
            edp: EdpPolicy::Auto,
            gpu: GpuPref::Auto,
            hooks: vec![],
        }
    }

    fn sig(descs: &[&str]) -> Vec<String> {
        descs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_select_profile_requires_all_matches() {
        let signature = sig(&["Dell U2723QE ABC123", "BOE 0x0BCA"]);
        let both = prof("both", &["Dell U2723QE", "BOE"], None);
        let other = prof("other", &["LG HDR 4K"], None);
        let profiles = vec![both.clone(), other];
        assert_eq!(select_profile(&signature, &profiles).unwrap().name, "both");
        let partial = sig(&["BOE 0x0BCA"]);
        assert!(select_profile(&partial, &[both]).is_none());
    }

    #[test]
    fn test_select_profile_specificity_then_explicit_priority() {
        let signature = sig(&["Dell U2723QE", "LG HDR 4K"]);
        let one = prof("one", &["Dell U2723QE"], None);
        let two = prof("two", &["Dell U2723QE", "LG HDR 4K"], None);
        let profiles = vec![one.clone(), two.clone()];
        // More matches wins (default priority = match count).
        assert_eq!(select_profile(&signature, &profiles).unwrap().name, "two");
        let pinned = prof("pinned", &["LG HDR 4K"], Some(99));
        let profiles = vec![one, two, pinned];
        assert_eq!(
            select_profile(&signature, &profiles).unwrap().name,
            "pinned"
        );
    }

    #[test]
    fn test_match_strips_desc_prefix_and_uses_startswith() {
        let signature = sig(&["Dell U2723QE HJKL (DP-3)"]);
        assert!(match_in_signature("desc:Dell U2723QE", &signature));
        assert!(match_in_signature("Dell U2723QE", &signature));
        assert!(!match_in_signature("U2723QE", &signature)); // not a prefix
    }

    fn mon(
        name: &str,
        desc: &str,
        x: i32,
        refresh: f64,
        scale: f64,
        disabled: bool,
    ) -> MonitorSnapshot {
        MonitorSnapshot {
            name: name.to_string(),
            description: desc.to_string(),
            width: 3840,
            height: 2160,
            refresh,
            x,
            y: 0,
            scale,
            transform: 0,
            disabled,
        }
    }

    #[test]
    fn test_render_profile_docked_layout() {
        // eDP enabled alongside externals: matches must cover EXTERNALS
        // only (lid close removes eDP from the signature) and the panel is
        // addressed by name, externals by desc — the dual-4k conventions.
        let monitors = vec![
            mon("eDP-2", "BOE 0x0BC9", 6144, 165.0, 1.25, false),
            mon("DP-1", "Dell B", 3072, 120.0, 1.25, false),
            mon("DP-4", "Dell A", 0, 120.0, 1.25, false),
        ];
        let (text, warnings) = render_profile(
            "desk",
            "2026-06-12",
            &monitors,
            EdpPolicy::Auto,
            GpuPref::Auto,
            None,
        )
        .unwrap();
        assert!(warnings.is_empty());
        let expected = "\
# Profile: desk — captured from the live layout (2026-06-12).
#
#@ match = desc:Dell A
#@ match = desc:Dell B
#@ edp = auto

monitor = desc:Dell A,3840x2160@120,0x0,1.25
monitor = desc:Dell B,3840x2160@120,3072x0,1.25
monitor = eDP-2,3840x2160@165,6144x0,1.25

# Add workspace pinning if desired, e.g.:
# workspace = 1, monitor:desc:..., default:true
";
        assert_eq!(text, expected);
        // Round-trip: the rendered profile must parse and self-match.
        let (profile, _) = parse_profile("desk", &text).unwrap();
        assert_eq!(profile.priority, 2); // implicit = match count
        let signature = sig(&["Dell A", "Dell B", "BOE 0x0BC9"]);
        assert!(
            profile
                .matches
                .iter()
                .all(|m| match_in_signature(m, &signature))
        );
    }

    #[test]
    fn test_render_profile_laptop_only_matches_edp() {
        let monitors = vec![mon("eDP-2", "BOE 0x0BC9", 0, 165.0, 1.25, false)];
        let (text, _) = render_profile(
            "mobile",
            "2026-06-12",
            &monitors,
            EdpPolicy::Auto,
            GpuPref::Auto,
            None,
        )
        .unwrap();
        assert!(text.contains("#@ match = desc:BOE 0x0BC9\n"));
        assert!(text.contains("monitor = eDP-2,3840x2160@165,0x0,1.25\n"));
    }

    #[test]
    fn test_render_profile_disabled_transform_and_directives() {
        let mut rotated = mon("DP-1", "Dell A", 0, 60.0, 1.0, false);
        rotated.transform = 1;
        let monitors = vec![rotated, mon("eDP-2", "BOE 0x0BC9", 1920, 165.0, 1.25, true)];
        let (text, _) = render_profile(
            "pivot",
            "2026-06-12",
            &monitors,
            EdpPolicy::Disable,
            GpuPref::Dgpu,
            Some(99),
        )
        .unwrap();
        assert!(text.contains("#@ edp = disable\n"));
        assert!(text.contains("#@ gpu = dgpu\n"));
        assert!(text.contains("#@ priority = 99\n"));
        assert!(text.contains("monitor = desc:Dell A,3840x2160@60,0x0,1,transform,1\n"));
        assert!(text.contains("monitor = eDP-2,disable\n"));
        let (profile, _) = parse_profile("pivot", &text).unwrap();
        assert_eq!(profile.priority, 99);
        assert_eq!(profile.gpu, GpuPref::Dgpu);
    }

    #[test]
    fn test_render_profile_comma_desc_falls_back_to_name() {
        let monitors = vec![mon("DP-3", "Weird, Inc. Display", 0, 60.0, 1.0, false)];
        let (text, warnings) = render_profile(
            "odd",
            "2026-06-12",
            &monitors,
            EdpPolicy::Auto,
            GpuPref::Auto,
            None,
        )
        .unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(text.contains("monitor = DP-3,"));
        // The match directive still uses the description (prefix-matched
        // against the signature, commas are fine there).
        assert!(text.contains("#@ match = desc:Weird, Inc. Display\n"));
    }

    #[test]
    fn test_render_profile_no_enabled_monitors_errors() {
        let monitors = vec![mon("eDP-2", "BOE", 0, 165.0, 1.25, true)];
        assert!(render_profile("x", "d", &monitors, EdpPolicy::Auto, GpuPref::Auto, None).is_err());
    }
}
