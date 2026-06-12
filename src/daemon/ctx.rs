//! Daemon context — owned EXCLUSIVELY by the dispatcher task. Event sources
//! never touch it; they send events. The one cross-task read is `locked`,
//! mirrored into a watch channel so the suspending tail can wait on it.

use std::time::Instant;

use tokio::task::JoinHandle;

use crate::pure::fsm::{ScreenInputs, ScreenState, State, WorldInputs};
use crate::pure::power::{PowerPolicy, PowerProfile, SelfWriteTracker};
use crate::pure::profiles::EdpPolicy;

pub struct Context {
    // ---- main + screen FSM inputs ----
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub logind_inhibitor: bool,
    pub wayland_inhibitor: bool,
    pub locked: bool,
    pub on_ac: bool,

    pub state: State,
    pub screen_state: ScreenState,

    // ---- timers (abort + respawn pattern) ----
    pub grace_timer: Option<JoinHandle<()>>,
    pub screen_timer: Option<JoinHandle<()>>,
    pub profile_debounce: Option<JoinHandle<()>>,
    pub power_debounce: Option<JoinHandle<()>>,

    // ---- monitor-profile sub-state ----
    pub current_profile: Option<String>,
    pub edp_policy: EdpPolicy,

    // ---- gpu drift detection ----
    /// None = unmanaged session (drift checks off); Some([]) = compositor
    /// defaults but advice still wanted (post transient/validation bail).
    pub gpu_actual: Option<Vec<String>>,
    pub gpu_actual_pending: bool,
    pub gpu_last_notified: Option<String>,
    pub gpu_last_notify_at: Option<Instant>,

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
            ext_mon_count: 0,
            logind_inhibitor: false,
            wayland_inhibitor: false,
            locked: false,
            on_ac: true,
            state: State::LidOpen,
            screen_state: ScreenState::Active,
            grace_timer: None,
            screen_timer: None,
            profile_debounce: None,
            power_debounce: None,
            current_profile: None,
            edp_policy: EdpPolicy::Auto,
            gpu_actual: None,
            gpu_actual_pending: true,
            gpu_last_notified: None,
            gpu_last_notify_at: None,
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
        }
    }

    pub fn screen_inputs(&self) -> ScreenInputs {
        ScreenInputs {
            locked: self.locked,
            inhibitor: self.inhibitor(),
        }
    }
}
