use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use super::logind_real_inhibitor_active;
use crate::daemon::event::{Event, ReconcileSnapshot};
use crate::paths;
use crate::sysio::{hyprctl, sysfs};
use logind_session::{LogindManagerProxy, LogindSessionProxy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileTrigger {
    Startup,
    MonitorHotplug,
    ConfigReloaded,
    ProfileApplied,
    LockChanged,
    SessionLock,
    SessionUnlock,
    Resumed,
    AcChanged,
    BatteryLowChanged,
    UeventsLost,
    SafetyTick,
}

impl ReconcileTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            ReconcileTrigger::Startup => "startup",
            ReconcileTrigger::MonitorHotplug => "monitor hotplug",
            ReconcileTrigger::ConfigReloaded => "configreloaded",
            ReconcileTrigger::ProfileApplied => "profile applied",
            ReconcileTrigger::LockChanged => "LockedHint changed",
            ReconcileTrigger::SessionLock => "session Lock",
            ReconcileTrigger::SessionUnlock => "session Unlock",
            ReconcileTrigger::Resumed => "resumed",
            ReconcileTrigger::AcChanged => "AC changed",
            ReconcileTrigger::BatteryLowChanged => "battery-low changed",
            ReconcileTrigger::UeventsLost => "uevents lost",
            ReconcileTrigger::SafetyTick => "safety tick",
        }
    }
}

pub fn describe(reasons: &[ReconcileTrigger]) -> String {
    let mut names: Vec<&'static str> = Vec::new();
    for r in reasons {
        if !names.contains(&r.as_str()) {
            names.push(r.as_str());
        }
    }
    names.join(", ")
}

pub async fn reconcile_task(
    tx: mpsc::Sender<Event>,
    mut triggers: mpsc::Receiver<ReconcileTrigger>,
    tick: watch::Sender<u64>,
    manager: LogindManagerProxy<'static>,
    session: Option<LogindSessionProxy<'static>>,
    mut ext_prev: u32,
    lid_present: bool,
) {
    loop {
        let first = tokio::select! {
            t = triggers.recv() => match t {
                Some(t) => t,
                None => return,
            },
            _ = tokio::time::sleep(paths::SAFETY_TICK) => ReconcileTrigger::SafetyTick,
        };
        if first == ReconcileTrigger::SafetyTick {
            tick.send_modify(|n| *n += 1);
        }
        let mut reasons = vec![first];
        let settle = tokio::time::sleep(paths::RECONCILE_SETTLE);
        tokio::pin!(settle);
        loop {
            tokio::select! {
                _ = &mut settle => break,
                t = triggers.recv() => match t {
                    Some(t) => reasons.push(t),
                    None => return,
                },
            }
        }
        debug!("reconcile pass ({})", describe(&reasons));
        let Some(snap) = gather(&manager, session.as_ref(), &mut ext_prev, lid_present).await
        else {
            continue;
        };
        let snap = ReconcileSnapshot {
            reasons: describe(&reasons),
            ..snap
        };
        if tx.send(Event::ReconcileTick(Box::new(snap))).await.is_err() {
            return;
        }
    }
}

async fn gather(
    manager: &LogindManagerProxy<'static>,
    session: Option<&LogindSessionProxy<'static>>,
    ext_prev: &mut u32,
    lid_present: bool,
) -> Option<ReconcileSnapshot> {
    let lid = if lid_present {
        match manager.lid_closed().await {
            Ok(v) => v,
            Err(e) => {
                warn!("reconciler snapshot failed: {e}");
                return None;
            }
        }
    } else {
        false
    };
    let monitors = hyprctl::monitors().await;
    let ext = hyprctl::ext_monitor_count_in(monitors.as_deref(), *ext_prev);
    *ext_prev = ext;
    let logind_inh = logind_real_inhibitor_active(manager).await.unwrap_or(false);
    let wayland_inh = hyprctl::wayland_idle_inhibited().await;
    let locked = match session {
        Some(s) => match s.locked_hint().await {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("reconciler LockedHint read failed: {e}");
                None
            }
        },
        None => None,
    };
    let on_ac = sysfs::on_ac_sysfs();
    let edp_disabled = monitors.as_deref().and_then(hyprctl::edp_is_disabled_in);
    let dpms_off = monitors
        .as_deref()
        .map(hyprctl::any_enabled_monitor_dpms_off_in);
    let cursor_pos = match dpms_off {
        Some(true) => hyprctl::cursor_pos().await,
        _ => None,
    };
    Some(ReconcileSnapshot {
        reasons: String::new(),
        lid_closed: lid,
        ext_mon_count: ext,
        logind_inhibitor: logind_inh,
        wayland_inhibitor: wayland_inh,
        locked,
        on_ac,
        edp_disabled,
        dpms_off,
        cursor_pos,
    })
}

#[cfg(test)]
mod tests {
    use super::{ReconcileTrigger, describe};

    #[test]
    fn describe_dedupes_a_burst() {
        let burst = [
            ReconcileTrigger::MonitorHotplug,
            ReconcileTrigger::MonitorHotplug,
            ReconcileTrigger::ProfileApplied,
        ];
        assert_eq!(describe(&burst), "monitor hotplug, profile applied");
    }
}
