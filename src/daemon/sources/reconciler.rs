//! The reconciler: gathers the world for the dispatcher to diff and repair.
//!
//! It used to run every 5 s. On a laptop that was one `hyprctl` child plus
//! three D-Bus round trips every five seconds for ever, which is exactly the
//! kind of wakeup a battery cannot afford. It now runs when something that
//! can move the world has happened (compositor events, lock and unlock,
//! resume, AC changes) and otherwise once per `SAFETY_TICK` as the net under
//! a missed signal. A burst of triggers coalesces into one pass.

use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use super::logind_real_inhibitor_active;
use crate::daemon::event::{Event, ReconcileSnapshot};
use crate::paths;
use crate::sysio::{hyprctl, sysfs};
use logind_session::{LogindManagerProxy, LogindSessionProxy};

/// Why a reconcile pass was asked for. Logged with the pass so drift can be
/// traced back to the event that exposed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileTrigger {
    MonitorHotplug,
    ConfigReloaded,
    ProfileApplied,
    LockChanged,
    SessionLock,
    SessionUnlock,
    Resumed,
    AcChanged,
    BatteryLowChanged,
    /// The uevent socket overflowed; something may have been missed.
    UeventsLost,
    SafetyTick,
}

impl ReconcileTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
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

/// Join a burst of triggers into one log-friendly reason string.
pub fn describe(reasons: &[ReconcileTrigger]) -> String {
    let mut names: Vec<&'static str> = Vec::new();
    for r in reasons {
        if !names.contains(&r.as_str()) {
            names.push(r.as_str());
        }
    }
    names.join(", ")
}

/// Wait for a trigger (or the safety tick), let the burst settle, gather one
/// snapshot and hand it to the dispatcher. `manager` and `session` must be
/// UNCACHED proxies: catching missed PropertiesChanged signals is this
/// task's whole purpose. `tick` is bumped on every safety tick so the
/// state-file task re-reads its inputs on the same cadence.
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
        // The tick is armed fresh after every pass, so it fires only after
        // SAFETY_TICK of nothing else having prompted a reconcile.
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
        // Coalesce: a hotplug is several socket2 lines, a profile apply, and
        // a reload within a second or two. One pass after the dust settles
        // sees the end state instead of three passes seeing three halves.
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

/// One world snapshot. None when the lid read failed on a lid-present
/// machine (a bad lid read must not fabricate a state).
async fn gather(
    manager: &LogindManagerProxy<'static>,
    session: Option<&LogindSessionProxy<'static>>,
    ext_prev: &mut u32,
    lid_present: bool,
) -> Option<ReconcileSnapshot> {
    // A lidless machine never reads the lid: there is nothing to read, and
    // a logind error here must not skip the pass that also repairs the
    // idle-suspend request.
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
    // One monitors payload per pass: ext count, eDP state and the
    // stuck-blank check all read the same JSON.
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
    // The cursor is only ever needed to decide whether a *dark* session
    // has a human in front of it; sample it only when something is blank.
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
