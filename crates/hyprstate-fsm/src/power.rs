//! Power policy: base states, the policy map, platform_profile value
//! mapping, battery-low hysteresis, and self-write bookkeeping (see
//! POWER_SPEC.md). Time-dependent functions take `Instant` parameters —
//! nothing here reads a clock.

use std::time::{Duration, Instant};

pub const BATTERY_LOW_EXIT_DELTA: u8 = 3;
pub const DEFAULT_BATTERY_LOW_PCT: u8 = 15;
/// Adoption suppression window after any ApplyProfile call.
pub const ADOPT_SUPPRESS: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerProfile {
    PowerSaver,
    Balanced,
    Performance,
}

impl PowerProfile {
    pub const ALL: [PowerProfile; 3] = [
        PowerProfile::PowerSaver,
        PowerProfile::Balanced,
        PowerProfile::Performance,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PowerProfile::PowerSaver => "power-saver",
            PowerProfile::Balanced => "balanced",
            PowerProfile::Performance => "performance",
        }
    }
}

/// The string was not one of `power-saver | balanced | performance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsePowerProfileError;

impl std::fmt::Display for ParsePowerProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("expected one of power-saver, balanced, performance")
    }
}

impl std::error::Error for ParsePowerProfileError {}

impl std::str::FromStr for PowerProfile {
    type Err = ParsePowerProfileError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "power-saver" => Ok(PowerProfile::PowerSaver),
            "balanced" => Ok(PowerProfile::Balanced),
            "performance" => Ok(PowerProfile::Performance),
            _ => Err(ParsePowerProfileError),
        }
    }
}

/// platform_profile value fallback chain per profile, validated against
/// `_choices` at apply time. Also the self-write acceptance set: ANY value
/// in a chain counts as our own write (quiet-only firmware writes "quiet"
/// when we asked for power-saver).
pub fn platform_profile_chain(p: PowerProfile) -> &'static [&'static str] {
    match p {
        PowerProfile::PowerSaver => &["low-power", "quiet"],
        PowerProfile::Balanced => &["balanced"],
        PowerProfile::Performance => &["performance"],
    }
}

/// Map an externally-written platform_profile value back to a profile
/// (adopt-don't-revert path).
pub fn profile_from_platform_value(value: Option<&str>) -> PowerProfile {
    match value {
        Some("low-power") | Some("quiet") => PowerProfile::PowerSaver,
        Some("performance") => PowerProfile::Performance,
        _ => PowerProfile::Balanced,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseState {
    DockedAc,
    Ac,
    Battery,
    BatteryLow,
}

impl BaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            BaseState::DockedAc => "docked-ac",
            BaseState::Ac => "ac",
            BaseState::Battery => "battery",
            BaseState::BatteryLow => "battery-low",
        }
    }
}

/// Pure: docked-ac | ac | battery | battery-low.
///
/// The AC axis is decided by `on_ac_settled` ALONE. Desktops never observe
/// an AC-unplug, so they sit permanently on the AC side without a special
/// case — while a laptop with UPower down (battery percentage unknowable)
/// still reaches the battery profiles via the reconciler's sysfs on_ac
/// repair. `low_battery` is never true without a known battery, so it alone
/// gates the low branch.
pub fn power_base_state(on_ac_settled: bool, ext_mon_count: u32, low_battery: bool) -> BaseState {
    if on_ac_settled {
        if ext_mon_count >= 1 {
            BaseState::DockedAc
        } else {
            BaseState::Ac
        }
    } else if low_battery {
        BaseState::BatteryLow
    } else {
        BaseState::Battery
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcAxis {
    Ac,
    Battery,
}

pub fn ac_axis(base: BaseState) -> AcAxis {
    match base {
        BaseState::Ac | BaseState::DockedAc => AcAxis::Ac,
        BaseState::Battery | BaseState::BatteryLow => AcAxis::Battery,
    }
}

/// Battery-low hysteresis step: enter at <= threshold, exit at >= threshold
/// + EXIT_DELTA, otherwise hold.
pub fn battery_low_step(low: bool, pct: f64, threshold: u8) -> bool {
    if !low && pct <= threshold as f64 {
        true
    } else if low && pct >= (threshold + BATTERY_LOW_EXIT_DELTA) as f64 {
        false
    } else {
        low
    }
}

/// base-state -> profile map from ~/.config/hypr/power.conf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerPolicy {
    pub docked_ac: PowerProfile,
    pub ac: PowerProfile,
    pub battery: PowerProfile,
    pub battery_low: PowerProfile,
}

impl Default for PowerPolicy {
    fn default() -> Self {
        PowerPolicy {
            docked_ac: PowerProfile::Balanced,
            ac: PowerProfile::Balanced,
            battery: PowerProfile::PowerSaver,
            battery_low: PowerProfile::PowerSaver,
        }
    }
}

impl PowerPolicy {
    pub fn for_base(&self, base: BaseState) -> PowerProfile {
        match base {
            BaseState::DockedAc => self.docked_ac,
            BaseState::Ac => self.ac,
            BaseState::Battery => self.battery,
            BaseState::BatteryLow => self.battery_low,
        }
    }
}

/// Parse power.conf text -> (policy, battery-low %, warnings). Missing keys
/// fall back to defaults; invalid values warn + keep the default. The io
/// layer handles the missing-file case (also defaults) and logs warnings.
pub fn parse_power_policy(text: &str) -> (PowerPolicy, u8, Vec<String>) {
    let mut policy = PowerPolicy::default();
    let mut low_pct = DEFAULT_BATTERY_LOW_PCT;
    let mut warnings = Vec::new();

    for line in text.lines() {
        if !line.starts_with("#@") {
            continue;
        }
        let Some((key, val)) = super::profiles::parse_directive(line, true) else {
            warnings.push(format!("ignoring malformed directive: {line:?}"));
            continue;
        };
        if key == "battery-low-percent" {
            match val.parse::<i64>() {
                Ok(n) => low_pct = n.clamp(1, 50) as u8,
                Err(_) => warnings.push(format!("bad battery-low-percent {val:?}")),
            }
            continue;
        }
        let slot = match key {
            "docked-ac" => &mut policy.docked_ac,
            "ac" => &mut policy.ac,
            "battery" => &mut policy.battery,
            "battery-low" => &mut policy.battery_low,
            other => {
                warnings.push(format!("unknown directive {other:?}"));
                continue;
            }
        };
        match val.parse::<PowerProfile>() {
            Ok(p) => *slot = p,
            Err(_) => warnings.push(format!(
                "{key} must be one of power-saver|balanced|performance, got {val:?} — using {}",
                slot.as_str()
            )),
        }
    }
    (policy, low_pct, warnings)
}

/// Self-write detection for platform_profile: tracks the expected values
/// (full fallback chain) of in-flight applies plus a suppression window
/// after any apply. Entries are registered BEFORE the ApplyProfile call so
/// the poller can't observe the sysfs change ahead of the bookkeeping.
#[derive(Debug, Default)]
pub struct SelfWriteTracker {
    pub entries: Vec<(Instant, &'static [&'static str])>,
    pub last_apply: Option<Instant>,
}

impl SelfWriteTracker {
    pub fn register(&mut self, now: Instant, profile: PowerProfile) {
        self.entries
            .push((now + ADOPT_SUPPRESS, platform_profile_chain(profile)));
        self.last_apply = Some(now);
    }

    /// True if a platform_profile change is one of our own in-flight applies
    /// (entry consumed) or inside the adoption-suppression window.
    pub fn is_self_write(&mut self, now: Instant, value: Option<&str>) -> bool {
        self.entries.retain(|(expiry, _)| *expiry > now);
        if let Some(v) = value
            && let Some(pos) = self.entries.iter().position(|(_, vals)| vals.contains(&v))
        {
            self.entries.remove(pos);
            return true;
        }
        matches!(self.last_apply, Some(t) if now.duration_since(t) < ADOPT_SUPPRESS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_power_base_state_ac_side() {
        assert_eq!(power_base_state(true, 0, false), BaseState::Ac);
        assert_eq!(power_base_state(true, 2, false), BaseState::DockedAc);
    }

    #[test]
    fn test_power_base_state_battery_side() {
        assert_eq!(power_base_state(false, 0, false), BaseState::Battery);
        assert_eq!(power_base_state(false, 0, true), BaseState::BatteryLow);
    }

    /// Regression (structural in v2): an unknown battery percentage (UPower
    /// down) must not pin the axis to AC — the axis is on_ac_settled alone,
    /// battery_percent is not even an input.
    #[test]
    fn test_power_base_state_upower_down_on_battery() {
        assert_eq!(power_base_state(false, 0, false), BaseState::Battery);
    }

    /// Desktops never observe an unplug: on_ac_settled stays true.
    #[test]
    fn test_power_base_state_desktop_stays_ac() {
        assert_eq!(power_base_state(true, 0, false), BaseState::Ac);
    }

    #[test]
    fn test_ac_axis() {
        assert_eq!(ac_axis(BaseState::Ac), AcAxis::Ac);
        assert_eq!(ac_axis(BaseState::DockedAc), AcAxis::Ac);
        assert_eq!(ac_axis(BaseState::Battery), AcAxis::Battery);
        assert_eq!(ac_axis(BaseState::BatteryLow), AcAxis::Battery);
    }

    #[test]
    fn test_profile_from_platform_value() {
        assert_eq!(
            profile_from_platform_value(Some("low-power")),
            PowerProfile::PowerSaver
        );
        assert_eq!(
            profile_from_platform_value(Some("quiet")),
            PowerProfile::PowerSaver
        );
        assert_eq!(
            profile_from_platform_value(Some("performance")),
            PowerProfile::Performance
        );
        assert_eq!(
            profile_from_platform_value(Some("balanced")),
            PowerProfile::Balanced
        );
        assert_eq!(
            profile_from_platform_value(Some("custom")),
            PowerProfile::Balanced
        );
        assert_eq!(profile_from_platform_value(None), PowerProfile::Balanced);
    }

    #[test]
    fn test_battery_low_hysteresis() {
        // Enter at <= threshold.
        assert!(battery_low_step(false, 15.0, 15));
        assert!(!battery_low_step(false, 16.0, 15));
        // Exit only at >= threshold + 3.
        assert!(battery_low_step(true, 17.0, 15));
        assert!(!battery_low_step(true, 18.0, 15));
    }

    #[test]
    fn test_power_self_write_matches_full_fallback_chain() {
        let t0 = Instant::now();
        let mut tracker = SelfWriteTracker::default();
        tracker.register(t0, PowerProfile::PowerSaver);
        // quiet-only firmware: we asked for power-saver, kernel wrote "quiet".
        assert!(tracker.is_self_write(t0 + Duration::from_secs(1), Some("quiet")));
        assert!(tracker.entries.is_empty()); // entry consumed
    }

    #[test]
    fn test_power_self_write_external_value_outside_window() {
        let t0 = Instant::now();
        let mut tracker = SelfWriteTracker {
            last_apply: Some(t0),
            ..Default::default()
        };
        assert!(!tracker.is_self_write(t0 + Duration::from_secs(100), Some("performance")));
    }

    #[test]
    fn test_power_self_write_suppression_window() {
        let t0 = Instant::now();
        let mut tracker = SelfWriteTracker::default();
        tracker.register(t0, PowerProfile::Balanced);
        // Any value inside the window counts as ours.
        assert!(tracker.is_self_write(t0 + Duration::from_secs(1), Some("performance")));
    }

    #[test]
    fn test_power_self_write_no_history_is_external() {
        let mut tracker = SelfWriteTracker::default();
        assert!(!tracker.is_self_write(Instant::now(), Some("performance")));
    }

    #[test]
    fn test_power_self_write_prunes_expired_entries() {
        let t0 = Instant::now();
        let mut tracker = SelfWriteTracker::default();
        tracker.register(t0, PowerProfile::PowerSaver);
        // 100s later the entry has expired: not a self write, entry pruned.
        assert!(!tracker.is_self_write(t0 + Duration::from_secs(100), Some("low-power")));
        assert!(tracker.entries.is_empty());
    }

    #[test]
    fn test_parse_power_policy_defaults_when_empty() {
        let (policy, pct, warnings) = parse_power_policy("");
        assert_eq!(policy, PowerPolicy::default());
        assert_eq!(pct, DEFAULT_BATTERY_LOW_PCT);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_parse_power_policy_parses_and_validates() {
        let (policy, pct, warnings) = parse_power_policy(
            "#@ docked-ac = performance\n\
             #@ battery = power-saver\n\
             #@ ac = warp-speed\n\
             #@ battery-low-percent = 80\n\
             #@ mystery-knob = 7\n",
        );
        assert_eq!(policy.docked_ac, PowerProfile::Performance);
        assert_eq!(policy.battery, PowerProfile::PowerSaver);
        assert_eq!(policy.ac, PowerProfile::Balanced); // invalid value -> default kept
        assert_eq!(pct, 50); // clamped to [1, 50]
        assert_eq!(warnings.len(), 2); // warp-speed + mystery-knob
    }

    #[test]
    fn test_policy_for_base() {
        let policy = PowerPolicy::default();
        assert_eq!(policy.for_base(BaseState::Ac), PowerProfile::Balanced);
        assert_eq!(
            policy.for_base(BaseState::BatteryLow),
            PowerProfile::PowerSaver
        );
    }
}
