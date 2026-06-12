//! The daemon's typed event enum (replaces v1's `Event(kind, payload)`).
//! Each variant projects to a `pure::fsm::EventKind` for the transition
//! maps and for v1-compatible log labels.

use crate::pure::fsm::EventKind;

#[derive(Debug)]
pub enum Event {
    /// LidClosed PropertiesChanged (true = closed).
    Lid(bool),
    /// monitoraddedv2 / monitorremoved from socket2.
    MonitorHotplug { added: bool, name: String },
    /// configreloaded from socket2 -> RECONCILE re-assert.
    ConfigReloaded,
    /// Debounced monitor-set change -> profile/gpu/power reconciliation.
    MonitorsChanged,
    /// Inhibitor poller edge. wayland=false -> logind source.
    Inhibitor { wayland: bool, active: bool },
    /// LockedHint PropertiesChanged (also mirrored into the locked watch).
    LockChanged(bool),
    /// Raw UPower OnBattery flip (true = on AC). Power policy consumes the
    /// debounced PowerAcSettled, never this.
    AcChanged(bool),
    /// The 5s AC settle window elapsed.
    PowerAcSettled,
    /// Grace timer fired.
    TimerExpired,
    /// Screen-DPMS timer fired.
    ScreenTimerExpired,
    /// PrepareForSleep(false).
    Resumed,
    /// 5s reconciler world snapshot; the dispatcher diffs and repairs.
    ReconcileTick(Box<ReconcileSnapshot>),
    /// platform_profile content changed (payload = first word).
    PlatformProfileChanged(Option<String>),
    /// gpu-select override file changed.
    GpuOverrideChanged(Option<String>),
    /// UPower DisplayDevice percentage sample.
    BatteryPercent(f64),
    /// power-override file changed (payload = first word).
    PowerOverrideChanged(Option<String>),
    /// org.hyprstate.Power1 (re)appeared on the bus.
    PowerdAppeared,
}

/// What the reconciler snapshot task gathered. Option fields = source
/// failed this pass (keep the previous ctx value).
#[derive(Debug, Default)]
pub struct ReconcileSnapshot {
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub logind_inhibitor: bool,
    pub wayland_inhibitor: bool,
    pub locked: bool,
    pub on_ac: Option<bool>,
    pub edp_disabled: Option<bool>,
}

impl Event {
    /// Projection for the pure transition maps + v1-style log labels.
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Lid(true) => EventKind::LidClose,
            Event::Lid(false) => EventKind::LidOpen,
            Event::MonitorHotplug { added: true, .. } => EventKind::MonitorAdded,
            Event::MonitorHotplug { added: false, .. } => EventKind::MonitorRemoved,
            Event::ConfigReloaded => EventKind::Reconcile,
            Event::MonitorsChanged => EventKind::MonitorsChanged,
            Event::Inhibitor { active: true, .. } => EventKind::InhibitorOn,
            Event::Inhibitor { active: false, .. } => EventKind::InhibitorOff,
            Event::LockChanged(true) => EventKind::LockEngaged,
            Event::LockChanged(false) => EventKind::LockReleased,
            Event::AcChanged(true) => EventKind::AcPlugged,
            Event::AcChanged(false) => EventKind::AcUnplugged,
            Event::PowerAcSettled => EventKind::PowerAcSettled,
            Event::TimerExpired => EventKind::TimerExpired,
            Event::ScreenTimerExpired => EventKind::ScreenTimerExpired,
            Event::Resumed => EventKind::Resumed,
            Event::ReconcileTick(_) => EventKind::CtxRepaired,
            Event::PlatformProfileChanged(_) => EventKind::PlatformProfileChanged,
            Event::GpuOverrideChanged(_) => EventKind::GpuOverrideChanged,
            Event::BatteryPercent(_) => EventKind::BatteryLowChanged,
            Event::PowerOverrideChanged(_) => EventKind::PowerOverrideChanged,
            Event::PowerdAppeared => EventKind::PowerAcSettled,
        }
    }

    /// v1 log label (EventKind value strings).
    pub fn label(&self) -> &'static str {
        match self.kind() {
            EventKind::LidClose => "LidClose",
            EventKind::LidOpen => "LidOpen",
            EventKind::MonitorAdded => "MonitorAdded",
            EventKind::MonitorRemoved => "MonitorRemoved",
            EventKind::InhibitorOn => "InhibitorOn",
            EventKind::InhibitorOff => "InhibitorOff",
            EventKind::LockEngaged => "LockEngaged",
            EventKind::LockReleased => "LockReleased",
            EventKind::AcPlugged => "AcPlugged",
            EventKind::AcUnplugged => "AcUnplugged",
            EventKind::TimerExpired => "TimerExpired",
            EventKind::ScreenTimerExpired => "ScreenTimerExpired",
            EventKind::Resumed => "Resumed",
            EventKind::Reconcile => "Reconcile",
            EventKind::MonitorsChanged => "MonitorsChanged",
            EventKind::CtxRepaired => "CtxRepaired",
            EventKind::PlatformProfileChanged => "PlatformProfileChanged",
            EventKind::GpuOverrideChanged => "GpuOverrideChanged",
            EventKind::BatteryLowChanged => "BatteryLowChanged",
            EventKind::PowerOverrideChanged => "PowerOverrideChanged",
            EventKind::PowerAcSettled => "PowerAcSettled",
        }
    }
}
