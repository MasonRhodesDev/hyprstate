//! Daemon context — owned EXCLUSIVELY by the dispatcher task. Event sources
//! never touch it; they send events. The one cross-task read is `locked`,
//! mirrored into a watch channel so the suspending tail can wait on it.

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
    // ---- main FSM + backstop inputs ----
    pub lid_closed: bool,
    /// Whether this machine is configured to have a lid at all
    /// (power.conf `#@ lid = present|absent`; default present). Daemon-side
    /// only: the FSM's lid route is naturally dead when lid_closed can
    /// never become true, so WorldInputs does not carry it. Gates the
    /// handle-lid-switch inhibitor, the lid watcher, and Lid events.
    pub lid_present: bool,
    /// power.conf `#@ lid-grace`: the lid-close countdown. Zero locks and
    /// suspends at once. Request-driven countdowns keep `GRACE_SECONDS`.
    pub lid_grace: Duration,
    /// power.conf `#@ lid-ac`: whether a closed lid on the charger suspends
    /// (default) or parks until the charger is pulled.
    pub lid_ac: LidAcPolicy,
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
    /// The Deferred-state sampler watching a Wayland claim for release.
    pub claim_watch: Option<JoinHandle<()>>,

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

    /// What waybar's power-profile module last rendered, as far as the
    /// daemon told it. None = not signalled yet this run.
    pub waybar_view: Option<WaybarView>,

    // ---- keyboard backlight (POWER_SPEC "Keyboard backlight") ----
    pub kbd_mode: KbdMode,
    pub kbd_idle: Duration,
    pub kbd_dark_lux: u32,
    /// `(leds name, max_brightness)`; None = no keyboard backlight LED.
    pub kbd_led: Option<(String, u32)>,
    /// Illuminance attribute of the ambient light sensor; None = no sensor.
    pub kbd_als: Option<PathBuf>,
    /// Input seen within the last `kbd_idle` (ext-idle-notify). Starts
    /// false: the first typing after the first idle period lights the keys.
    pub kbd_typing: bool,
    /// PrepareForSleep(true) seen, no Resumed yet.
    pub kbd_sleeping: bool,
    pub kbd_dark: bool,
    pub kbd_last_lux: Option<f64>,
    /// Percent the keys are lit at (the kbd-level file, else the default).
    pub kbd_level: u8,
    /// The runtime override marker is present.
    pub kbd_override: bool,
    /// Raw value last written to the LED; None = not written this run.
    pub kbd_applied: Option<u32>,
}

/// Everything `hyprstate power status --waybar` renders. The module runs
/// its exec only on SIGRTMIN+9 (hypr-DE `"signal": 9`, `interval: once`),
/// so any change here must signal it, not only a new applied profile:
/// `power set` to the profile already applied flips the override class,
/// and powerd going away or coming back flips the unavailable class.
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
    /// The keyboard backlight policy acts only with `kbd = auto`, an LED and
    /// a sensor. Anything less is inert: logged once, never written.
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
            // battery_low is a bypass, not an axis: only meaningful on
            // battery. The settled AC signal, so a plug-in during the 30 s
            // grace (settles in ~5 s) withdraws the bypass before expiry.
            battery_low: self.low_battery && !self.on_ac_settled,
            // The raw AC signal: pulling the charger from a parked lid
            // should start the countdown now, and the grace absorbs a
            // plug jiggle on its own.
            lid_ac_hold: self.lid_ac == LidAcPolicy::Defer && self.on_ac,
        }
    }
}
