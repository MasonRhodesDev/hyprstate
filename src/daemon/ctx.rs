use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::pure::fsm::{State, WorldInputs};
use crate::pure::kbd::{DEFAULT_KBD_DARK_LUX, DEFAULT_KBD_IDLE_SECS, DEFAULT_KBD_LEVEL_PCT};
use crate::pure::power::{
    DEFAULT_LID_GRACE_SECS, KbdMode, LidAcPolicy, PowerPolicy, PowerProfile, SelfWriteTracker,
};
use crate::pure::profiles::EdpPolicy;

pub struct Context {
    pub lid_closed: bool,
    pub lid_present: bool,
    pub lid_grace: Duration,
    pub lid_ac: LidAcPolicy,
    pub ext_mon_count: u32,
    pub logind_inhibitor: bool,
    pub wayland_inhibitor: bool,
    pub locked: bool,
    pub on_ac: bool,

    pub state: State,
    pub suspend_requested: bool,
    pub state_span: Option<tracing::Span>,

    pub grace_timer: Option<JoinHandle<()>>,
    pub profile_debounce: Option<JoinHandle<()>>,
    pub power_debounce: Option<JoinHandle<()>>,
    pub claim_watch: Option<JoinHandle<()>>,

    pub last_cursor_pos: Option<(i64, i64)>,

    pub current_profile: Option<String>,
    pub active_profile_rev: Option<monitor_profiles::Profile>,
    pub edp_policy: EdpPolicy,

    pub gpu_actual: Option<Vec<String>>,
    pub gpu_actual_pending: bool,
    pub gpu_last_notified: Option<String>,
    pub gpu_last_notify_at: Option<Instant>,
    pub dgpu_pinned: Option<bool>,

    pub on_ac_settled: bool,
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

    pub brightness_dev: Option<String>,
    pub brightness_max: u32,
    pub brightness_set: Option<u32>,
    pub brightness_saved: Option<u32>,

    pub waybar_view: Option<WaybarView>,

    pub kbd_mode: KbdMode,
    pub kbd_idle: Duration,
    pub kbd_dark_lux: u32,
    pub kbd_led: Option<(String, u32)>,
    pub kbd_als: Option<PathBuf>,
    pub kbd_typing: bool,
    pub kbd_sleeping: bool,
    pub kbd_dark: bool,
    pub kbd_last_lux: Option<f64>,
    pub kbd_level: u8,
    pub kbd_override: bool,
    pub kbd_applied: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaybarView {
    pub applied: Option<PowerProfile>,
    pub override_set: bool,
    pub powerd_available: bool,
}

impl WaybarView {
    pub fn of(ctx: &Context) -> Self {
        WaybarView {
            applied: ctx.power_applied,
            override_set: ctx.power_override.is_some(),
            powerd_available: ctx.powerd_available,
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Context {
            lid_closed: false,
            lid_present: true,
            lid_grace: Duration::from_secs(DEFAULT_LID_GRACE_SECS as u64),
            lid_ac: LidAcPolicy::Suspend,
            ext_mon_count: 0,
            logind_inhibitor: false,
            wayland_inhibitor: false,
            locked: false,
            on_ac: true,
            state: State::LidOpen,
            suspend_requested: false,
            state_span: None,
            grace_timer: None,
            last_cursor_pos: None,
            profile_debounce: None,
            power_debounce: None,
            claim_watch: None,
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
            waybar_view: None,
            kbd_mode: KbdMode::Auto,
            kbd_idle: Duration::from_secs(DEFAULT_KBD_IDLE_SECS as u64),
            kbd_dark_lux: DEFAULT_KBD_DARK_LUX,
            kbd_led: None,
            kbd_als: None,
            kbd_typing: false,
            kbd_sleeping: false,
            kbd_dark: false,
            kbd_last_lux: None,
            kbd_level: DEFAULT_KBD_LEVEL_PCT,
            kbd_override: false,
            kbd_applied: None,
        }
    }
}

impl Context {
    pub fn kbd_active(&self) -> bool {
        self.kbd_mode == KbdMode::Auto && self.kbd_led.is_some() && self.kbd_als.is_some()
    }

    pub fn inhibitor(&self) -> bool {
        self.logind_inhibitor || self.wayland_inhibitor
    }

    pub fn world(&self) -> WorldInputs {
        WorldInputs {
            lid_closed: self.lid_closed,
            ext_mon_count: self.ext_mon_count,
            inhibitor: self.inhibitor(),
            suspend_requested: self.suspend_requested,
            locked: self.locked,
            battery_low: self.low_battery && !self.on_ac_settled,
            lid_ac_hold: self.lid_ac == LidAcPolicy::Defer && self.on_ac,
        }
    }
}
