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
}
