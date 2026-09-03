//! Daemon context — owned EXCLUSIVELY by the dispatcher task. Event sources
//! never touch it; they send events. The one cross-task read is `locked`,
//! mirrored into a watch channel so the suspending tail can wait on it.

use std::time::Instant;

use tokio::task::JoinHandle;

use crate::pure::fsm::{State, WorldInputs};
use crate::pure::power::{PowerPolicy, PowerProfile, SelfWriteTracker};
use crate::pure::profiles::EdpPolicy;

pub struct Context {
    // ---- main FSM + backstop inputs ----
    pub lid_closed: bool,
    /// Whether this machine is configured to have a lid at all
    /// (power.conf `#@ lid = present|absent`; default present). Daemon-side
    /// only: the FSM's lid route is naturally dead when lid_closed can
    /// never become true, so WorldInputs does not carry it. Gates the
    /// handle-lid-switch inhibitor, the lid watcher, and Lid events.
    pub lid_present: bool,
    pub ext_mon_count: u32,
    pub logind_inhibitor: bool,
    pub wayland_inhibitor: bool,
    pub locked: bool,
    pub on_ac: bool,

    pub state: State,
    /// A standing idle-suspend request (runtime request file present).
    /// Cleared on Resumed before dispatch - a stale value re-enters
    /// Countdown after wake and loops the machine back into suspend.
    pub suspend_requested: bool,

    // ---- timers (abort + respawn pattern) ----
    pub grace_timer: Option<JoinHandle<()>>,
    pub profile_debounce: Option<JoinHandle<()>>,
    pub power_debounce: Option<JoinHandle<()>>,

    /// Cursor position at the previous reconciler pass; a change is the
    /// presence signal for the stuck-DPMS backstop. None = not sampled yet.
    pub last_cursor_pos: Option<(i64, i64)>,

    // ---- monitor-profile sub-state ----
    pub current_profile: Option<String>,
    /// Last applied profile body — same name with different TOML must re-apply.
    pub active_profile_rev: Option<monitor_profiles::Profile>,
    pub edp_policy: EdpPolicy,

    // ---- gpu drift detection ----
    /// None = unmanaged session (drift checks off); Some([]) = compositor
    /// defaults but advice still wanted (post transient/validation bail).
    pub gpu_actual: Option<Vec<String>>,
    pub gpu_actual_pending: bool,
    pub gpu_last_notified: Option<String>,
    pub gpu_last_notify_at: Option<Instant>,
    /// Last dgpu runtime-PM pin pushed to powerd (idempotence). None = never
    /// pushed; reset on PowerdAppeared so it re-pushes after powerd restarts.
    pub dgpu_pinned: Option<bool>,

    // ---- power policy ----
    pub on_ac_settled: bool,
    /// None = no battery (or UPower down) — low-battery machinery off.
    pub battery_percent: Option<f64>,
    pub low_battery: bool,
    pub power_policy: PowerPolicy,
    pub battery_low_pct: u8,
    pub power_override: Option<PowerProfile>,
    pub power_override_base: Option<crate::pure::power::BaseState>,
    pub power_applied: Option<PowerProfile>,
    pub power_last_base: Option<crate::pure::power::BaseState>,
    pub self_writes: SelfWriteTracker,
    pub powerd_available: bool,
    pub powerd_warned: bool,

    // ---- brightness ----
    pub brightness_dev: Option<String>,
    pub brightness_max: u32,
    pub brightness_set: Option<u32>,
    pub brightness_saved: Option<u32>,
}

impl Default for Context {
    fn default() -> Self {
        Context {
            lid_closed: false,
            lid_present: true,
            ext_mon_count: 0,
            logind_inhibitor: false,
            wayland_inhibitor: false,
            locked: false,
            on_ac: true,
            state: State::LidOpen,
            suspend_requested: false,
            grace_timer: None,
            last_cursor_pos: None,
            profile_debounce: None,
            power_debounce: None,
            current_profile: None,
            active_profile_rev: None,
            edp_policy: EdpPolicy::Auto,
            gpu_actual: None,
            gpu_actual_pending: true,
            gpu_last_notified: None,
            gpu_last_notify_at: None,
            dgpu_pinned: None,
            on_ac_settled: true,
            battery_percent: Some(100.0),
            low_battery: false,
            power_policy: PowerPolicy::default(),
            battery_low_pct: crate::pure::power::DEFAULT_BATTERY_LOW_PCT,
            power_override: None,
            power_override_base: None,
            power_applied: None,
            power_last_base: None,
            self_writes: SelfWriteTracker::default(),
            powerd_available: true,
            powerd_warned: false,
            brightness_dev: None,
            brightness_max: 0,
            brightness_set: None,
            brightness_saved: None,
        }
    }
}

impl Context {
    pub fn inhibitor(&self) -> bool {
        self.logind_inhibitor || self.wayland_inhibitor
    }

    pub fn world(&self) -> WorldInputs {
        WorldInputs {
            lid_closed: self.lid_closed,
            ext_mon_count: self.ext_mon_count,
            inhibitor: self.inhibitor(),
            suspend_requested: self.suspend_requested,
        }
    }
}
