use crate::pure::fsm::EventKind;

#[derive(Debug)]
pub enum Event {
    Lid(bool),
    MonitorHotplug { added: bool, name: String },
    ConfigReloaded,
    ProfileApplied,
    MonitorsChanged,
    ProfilesChanged,
    Inhibitor { wayland: bool, active: bool },
    LockChanged(bool),
    AcChanged(bool),
    PowerAcSettled,
    TimerExpired,
    Resumed,
    ReconcileTick(Box<ReconcileSnapshot>),
    PlatformProfileChanged(Option<String>),
    GpuOverrideChanged(Option<String>),
    BatteryPercent(f64),
    PowerOverrideChanged(Option<String>),
    SuspendRequestChanged(Option<String>),
    PowerdAppeared,
    BacklightHotplug,
    Sleeping,
    KbdIdle { idle: bool },
    KbdLevelChanged(Option<u8>),
    KbdOverrideChanged(bool),
    KbdLedHotplug,
}

#[derive(Debug, Default)]
pub struct ReconcileSnapshot {
    pub reasons: String,
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub logind_inhibitor: bool,
    pub wayland_inhibitor: Option<bool>,
    pub locked: Option<bool>,
    pub on_ac: Option<bool>,
    pub edp_disabled: Option<bool>,
    pub dpms_off: Option<bool>,
    pub cursor_pos: Option<(i64, i64)>,
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Lid(true) => EventKind::LidClose,
            Event::Lid(false) => EventKind::LidOpen,
            Event::MonitorHotplug { added: true, .. } => EventKind::MonitorAdded,
            Event::MonitorHotplug { added: false, .. } => EventKind::MonitorRemoved,
            Event::ConfigReloaded | Event::ProfileApplied => EventKind::Reconcile,
            Event::MonitorsChanged | Event::ProfilesChanged => EventKind::MonitorsChanged,
            Event::Inhibitor { active: true, .. } => EventKind::InhibitorOn,
            Event::Inhibitor { active: false, .. } => EventKind::InhibitorOff,
            Event::LockChanged(true) => EventKind::LockEngaged,
            Event::LockChanged(false) => EventKind::LockReleased,
            Event::AcChanged(true) => EventKind::AcPlugged,
            Event::AcChanged(false) => EventKind::AcUnplugged,
            Event::PowerAcSettled => EventKind::PowerAcSettled,
            Event::TimerExpired => EventKind::TimerExpired,
            Event::Resumed => EventKind::Resumed,
            Event::ReconcileTick(_) => EventKind::CtxRepaired,
            Event::PlatformProfileChanged(_) => EventKind::PlatformProfileChanged,
            Event::GpuOverrideChanged(_) => EventKind::GpuOverrideChanged,
            Event::BatteryPercent(_) => EventKind::BatteryLowChanged,
            Event::PowerOverrideChanged(_) => EventKind::PowerOverrideChanged,
            Event::SuspendRequestChanged(Some(_)) => EventKind::SuspendRequested,
            Event::SuspendRequestChanged(None) => EventKind::SuspendCancelled,
            Event::PowerdAppeared => EventKind::PowerAcSettled,
            Event::BacklightHotplug
            | Event::Sleeping
            | Event::KbdIdle { .. }
            | Event::KbdLevelChanged(_)
            | Event::KbdOverrideChanged(_)
            | Event::KbdLedHotplug => EventKind::CtxRepaired,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Event::ProfilesChanged => return "ProfilesChanged",
            Event::BacklightHotplug => return "BacklightHotplug",
            Event::Sleeping => return "Sleeping",
            Event::KbdIdle { idle: true } => return "KbdIdled",
            Event::KbdIdle { idle: false } => return "KbdResumed",
            Event::KbdLevelChanged(_) => return "KbdLevelChanged",
            Event::KbdOverrideChanged(_) => return "KbdOverrideChanged",
            Event::KbdLedHotplug => return "KbdLedHotplug",
            _ => {}
        }
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
            EventKind::Resumed => "Resumed",
            EventKind::Reconcile => "Reconcile",
            EventKind::MonitorsChanged => "MonitorsChanged",
            EventKind::CtxRepaired => "CtxRepaired",
            EventKind::PlatformProfileChanged => "PlatformProfileChanged",
            EventKind::GpuOverrideChanged => "GpuOverrideChanged",
            EventKind::BatteryLowChanged => "BatteryLowChanged",
            EventKind::PowerOverrideChanged => "PowerOverrideChanged",
            EventKind::PowerAcSettled => "PowerAcSettled",
            EventKind::SuspendRequested => "SuspendRequested",
            EventKind::SuspendCancelled => "SuspendCancelled",
        }
    }
}
