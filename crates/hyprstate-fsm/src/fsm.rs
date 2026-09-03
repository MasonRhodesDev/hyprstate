//! Main lid/suspend FSM and the stuck-DPMS backstop.
//!
//! Port of hyprstate.py's `desired_state` / `_world_state`. The functions are
//! total over plain input structs; the daemon snapshots its `Context` into
//! `WorldInputs` / `StuckScreenInputs` at dispatch time. hyprstate has no
//! DPMS-off decision: hypridle owns blanking (hyprstate#24).

/// Main FSM states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum State {
    LidOpen,
    Docked,
    Deferred,
    Countdown,
    Suspending,
}

impl State {
    /// Log labels matching v1 so journals stay diffable across the port.
    pub fn as_str(self) -> &'static str {
        match self {
            State::LidOpen => "LID_OPEN",
            State::Docked => "DOCKED",
            State::Deferred => "DEFERRED",
            State::Countdown => "COUNTDOWN",
            State::Suspending => "SUSPENDING",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EventKind {
    LidClose,
    LidOpen,
    MonitorAdded,
    MonitorRemoved,
    InhibitorOn,
    InhibitorOff,
    LockEngaged,
    LockReleased,
    AcPlugged,
    AcUnplugged,
    TimerExpired,
    Resumed,
    /// An idle-suspend request appeared (hypridle's timeout fired).
    SuspendRequested,
    /// The standing request was withdrawn (input, or explicit cancel).
    SuspendCancelled,
    Reconcile,
    MonitorsChanged,
    CtxRepaired,
    PlatformProfileChanged,
    GpuOverrideChanged,
    BatteryLowChanged,
    PowerOverrideChanged,
    PowerAcSettled,
}

/// The world inputs `world_state` derives from.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorldInputs {
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub inhibitor: bool,
    /// An idle-suspend request is standing (the runtime request file
    /// exists). The daemon clears it on Resumed and on cancellation;
    /// leaving it set across a resume would re-enter Countdown and loop.
    pub suspend_requested: bool,
}

pub fn world_state(w: &WorldInputs) -> State {
    // Docked (lid shut, driving external monitors) is a deliberate
    // "stay awake" - a laptop used as a workstation - and outranks an
    // idle-suspend request: a docked laptop mid-build must not idle-suspend,
    // exactly as before this feature. A lidless desktop is NEVER Docked
    // (lid_closed is forced false when lid=absent, so it is LidOpen), so
    // this early return does not block idle-suspend there - which is the
    // whole point. Behaviour-preserving for the no-request path: every case
    // that reached Docked below still does.
    if w.lid_closed && w.ext_mon_count >= 1 {
        return State::Docked;
    }
    // A standing request outranks the remaining lid chain (a desktop is
    // otherwise LidOpen forever), but NOT an inhibitor - a request made
    // while media plays parks in Deferred, exactly as a closed lid would,
    // and proceeds when the inhibitor drops. Everything downstream (grace,
    // lock proof, cancellation, the single do_suspend site) is the same
    // machinery the lid uses; this only adds a way in.
    if w.suspend_requested {
        return if w.inhibitor {
            State::Deferred
        } else {
            State::Countdown
        };
    }
    if !w.lid_closed {
        State::LidOpen
    } else if w.ext_mon_count >= 1 {
        State::Docked
    } else if w.inhibitor {
        State::Deferred
    } else {
        State::Countdown
    }
}

/// Pure main-FSM transition. `None` = stay put.
pub fn desired_state(state: State, ev: EventKind, w: &WorldInputs) -> Option<State> {
    if ev == EventKind::TimerExpired {
        if state != State::Countdown {
            return None;
        }
        // Re-derive before suspending: if inputs were repaired behind the
        // FSM's back (reconciler drift — e.g. a missed LidClosed change), no
        // transition fired and the grace timer was never cancelled. A stale
        // timer must not suspend a machine whose world says LID_OPEN.
        let target = world_state(w);
        return Some(if target == State::Countdown {
            State::Suspending
        } else {
            target
        });
    }

    if ev == EventKind::Resumed {
        return (state == State::Suspending).then(|| world_state(w));
    }

    if state == State::Suspending {
        return None;
    }

    let target = world_state(w);
    (target != state).then_some(target)
}

/// Inputs of the stuck-DPMS backstop (see `dpms_stuck_off`).
#[derive(Debug, Clone, Copy, Default)]
pub struct StuckScreenInputs {
    /// Reality: at least one ENABLED output reports DPMS off.
    pub dpms_off: bool,
    pub locked: bool,
    /// Positive evidence a human is present: the cursor moved between
    /// reconciler passes. Required before repairing a *locked* dark session,
    /// so an ordinary idle blank is not undone seconds after hypridle made it.
    pub cursor_moved: bool,
}

/// Whether an observed DPMS-off state is *unowned* and must be repaired.
///
/// hypridle, not hyprstate, does every blank -- the unlocked idle listener
/// and the locked input-idle listener in hypr-DE's hypridle.conf -- so an
/// observed DPMS off is normally legitimate and must be left alone (v1 fired
/// dpms(on) on every config reload and fought hypridle for exactly this
/// reason; 2.x blanked locked sessions itself on an input-blind timer and
/// re-blanked them under the user's hands, hyprstate#24). Turning outputs
/// on is the only DPMS effect hyprstate has.
///
/// This backstop exists because hypridle can *lose* its wake: `CHypridle::
/// onInhibit` recreates the idle-notify listeners when the systemd idle
/// inhibit count returns to 0, clearing `isIdled` without ever running
/// `on-resume`, and `CHypridle::onResumed` early-returns while any inhibit
/// lock is held. Either path drops the `dpms on` permanently and never
/// retries, leaving a live compositor driving dark panels that no amount of
/// input will wake.
///
/// The guard is what keeps this from fighting a legitimate blank:
/// - Only LidOpen/Docked are meant to be showing anything; a machine on its
///   way to suspend stays dark.
/// - Unlocked + dark is unambiguous: hypridle locks at 180s and blanks at
///   240s, so it never blanks a session it has not already locked.
/// - Locked + dark needs positive evidence a human is present, or every
///   ordinary idle blank would be undone 5 seconds later.
///
/// Known gap: cursor movement is the only presence signal available without
/// hyprstate becoming a Wayland client itself, so a keyboard-only wake of a
/// *locked* session is not covered.
pub fn dpms_stuck_off(main: State, s: &StuckScreenInputs) -> bool {
    if !s.dpms_off {
        return false;
    }
    if !matches!(main, State::LidOpen | State::Docked) {
        return false;
    }
    !s.locked || s.cursor_moved
}

/// Whether the internal panel may be turned off.
///
/// Never when it is the only output. Countdown turns the panel off on lid
/// close, which is right while docked and catastrophic while not: removing
/// the last monitor leaves the compositor with none, and Hyprland segfaults
/// refocusing windows onto a monitor that no longer exists (observed
/// repeatedly, `CWorkspace::isVisibleNotCovered` via
/// `CInputManager::refocusLastWindow` from a layer surface unmapping).
///
/// Nothing is lost by keeping it: the machine is seconds from suspending,
/// and blanking is DPMS's job, not modesetting's.
pub fn edp_may_disable(ext_mon_count: u32) -> bool {
    ext_mon_count > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(lid_closed: bool, ext_mon_count: u32, inhibitor: bool) -> WorldInputs {
        WorldInputs {
            lid_closed,
            ext_mon_count,
            inhibitor,
            suspend_requested: false,
        }
    }

    /// An idle-suspend request on a lidless desktop: lid open, external
    /// monitors present, request standing. `inhibitor` varies the branch.
    fn req(inhibitor: bool) -> WorldInputs {
        WorldInputs {
            lid_closed: false,
            ext_mon_count: 2,
            inhibitor,
            suspend_requested: true,
        }
    }

    /// Drive a sequence of (event, world, expected desired_state) from a
    /// start state, threading the state forward. Sequences catch the
    /// stale-timer / stale-request classes that a per-call value grid
    /// cannot: the bug lives in what yesterday's state was.
    fn drive(start: State, steps: &[(EventKind, WorldInputs, Option<State>)]) -> State {
        let mut state = start;
        for (i, (ev, world, expect)) in steps.iter().enumerate() {
            let got = desired_state(state, *ev, world);
            assert_eq!(got, *expect, "step {i}: {ev:?} from {state:?}");
            if let Some(next) = got {
                state = next;
            }
        }
        state
    }

    #[test]
    fn test_world_state() {
        for (inputs, expected) in [
            (w(false, 0, false), State::LidOpen),
            (w(false, 2, false), State::LidOpen),
            (w(true, 1, false), State::Docked),
            (w(true, 0, true), State::Deferred),
            (w(true, 0, false), State::Countdown),
        ] {
            assert_eq!(world_state(&inputs), expected);
        }
    }

    #[test]
    fn test_event_moves_to_world_state() {
        let inputs = w(true, 0, false);
        assert_eq!(
            desired_state(State::LidOpen, EventKind::LidClose, &inputs),
            Some(State::Countdown)
        );
    }

    #[test]
    fn test_same_state_is_none() {
        let inputs = w(false, 0, false);
        assert_eq!(
            desired_state(State::LidOpen, EventKind::Reconcile, &inputs),
            None
        );
    }

    #[test]
    fn test_suspending_ignores_world_events() {
        let inputs = w(false, 0, false);
        assert_eq!(
            desired_state(State::Suspending, EventKind::LidOpen, &inputs),
            None
        );
    }

    #[test]
    fn test_resumed_rederives_from_suspending_only() {
        let inputs = w(false, 0, false);
        assert_eq!(
            desired_state(State::Suspending, EventKind::Resumed, &inputs),
            Some(State::LidOpen)
        );
        assert_eq!(
            desired_state(State::LidOpen, EventKind::Resumed, &inputs),
            None
        );
    }

    #[test]
    fn test_timer_expired_suspends_from_countdown() {
        let inputs = w(true, 0, false);
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &inputs),
            Some(State::Suspending)
        );
    }

    #[test]
    fn test_timer_expired_ignored_outside_countdown() {
        let inputs = w(true, 0, false);
        for state in [
            State::LidOpen,
            State::Docked,
            State::Deferred,
            State::Suspending,
        ] {
            assert_eq!(desired_state(state, EventKind::TimerExpired, &inputs), None);
        }
    }

    /// Regression: reconciler repaired lid_closed behind the FSM's back, so
    /// no transition cancelled the grace timer. Expiry must re-derive, not
    /// suspend.
    #[test]
    fn test_stale_timer_must_not_suspend_lid_open_machine() {
        let inputs = w(false, 0, false);
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &inputs),
            Some(State::LidOpen)
        );
    }

    #[test]
    fn test_stale_timer_rederives_docked_and_deferred() {
        assert_eq!(
            desired_state(
                State::Countdown,
                EventKind::TimerExpired,
                &w(true, 1, false)
            ),
            Some(State::Docked)
        );
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &w(true, 0, true)),
            Some(State::Deferred)
        );
    }

    #[test]
    fn test_ctx_repaired_drives_transition() {
        let inputs = w(true, 0, false);
        assert_eq!(
            desired_state(State::LidOpen, EventKind::CtxRepaired, &inputs),
            Some(State::Countdown)
        );
    }

    #[test]
    fn edp_stays_on_when_it_is_the_only_output() {
        assert!(
            !edp_may_disable(0),
            "disabling the sole output leaves the compositor with no monitors"
        );
    }

    #[test]
    fn edp_may_disable_once_an_external_is_present() {
        assert!(edp_may_disable(1));
        assert!(edp_may_disable(3));
    }

    fn stuck(dpms_off: bool, locked: bool, cursor_moved: bool) -> StuckScreenInputs {
        StuckScreenInputs {
            dpms_off,
            locked,
            cursor_moved,
        }
    }

    #[test]
    fn test_stuck_dpms_ignores_screens_that_are_on() {
        assert!(!dpms_stuck_off(State::LidOpen, &stuck(false, false, true)));
    }

    #[test]
    fn test_stuck_dpms_leaves_an_ordinary_idle_blank_alone() {
        // hypridle's locked listener blanked the session and the user is
        // still away: no cursor movement, so nothing to repair.
        assert!(!dpms_stuck_off(State::Docked, &stuck(true, true, false)));
    }

    #[test]
    fn test_stuck_dpms_repairs_locked_session_once_user_returns() {
        // The incident: hypridle dropped on-resume, the panels stayed dark,
        // and input reached the compositor without waking anything.
        assert!(dpms_stuck_off(State::Docked, &stuck(true, true, true)));
    }

    #[test]
    fn test_stuck_dpms_repairs_unlocked_dark_session_without_cursor_proof() {
        // hypridle locks at 180s before its unlocked listener blanks at 240s,
        // and its locked listener needs the compositor lock, so an unlocked
        // session is never blanked on purpose — unlocked + dark is repairable.
        assert!(dpms_stuck_off(State::LidOpen, &stuck(true, false, false)));
    }

    #[test]
    fn test_stuck_dpms_stays_dark_on_the_way_to_suspend() {
        for main in [State::Countdown, State::Deferred, State::Suspending] {
            assert!(
                !dpms_stuck_off(main, &stuck(true, false, true)),
                "{} must not wake screens",
                main.as_str()
            );
        }
    }

    #[test]
    fn suspend_request_wins_over_lid_chain() {
        // The desktop case: lid open (would be LidOpen), external monitors
        // (would be Docked), but a request stands -> Countdown.
        assert_eq!(world_state(&req(false)), State::Countdown);
    }

    #[test]
    fn a_docked_laptop_with_a_request_stays_docked() {
        // Review #6: a docked laptop (lid shut + externals) is deliberately
        // kept awake; an idle request must not suspend it. A lidless desktop
        // is LidOpen, not Docked, so this does not affect it.
        let docked_request = WorldInputs {
            lid_closed: true,
            ext_mon_count: 2,
            inhibitor: false,
            suspend_requested: true,
        };
        assert_eq!(world_state(&docked_request), State::Docked);
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &docked_request),
            Some(State::Docked),
            "a docked laptop must re-derive out of Countdown, never suspend"
        );
    }

    #[test]
    fn suspend_request_with_inhibitor_is_deferred() {
        // A request made while media plays parks, exactly as a closed lid
        // would, and proceeds when the inhibitor drops.
        assert_eq!(world_state(&req(true)), State::Deferred);
    }

    #[test]
    fn suspend_request_enters_countdown_from_any_awake_state() {
        for start in [State::LidOpen, State::Docked, State::Deferred] {
            assert_eq!(
                desired_state(start, EventKind::SuspendRequested, &req(false)),
                Some(State::Countdown),
                "from {start:?}"
            );
        }
    }

    #[test]
    fn suspend_request_ignored_while_suspending() {
        assert_eq!(
            desired_state(State::Suspending, EventKind::SuspendRequested, &req(false)),
            None
        );
    }

    #[test]
    fn suspend_cancel_returns_to_world_state() {
        // Request withdrawn (suspend_requested now false): from Countdown
        // the world is a docked desktop.
        // Lidless desktop: lid open wins the chain, so withdrawing the
        // request rests at LidOpen (not Docked - that needs a closed lid).
        assert_eq!(
            desired_state(
                State::Countdown,
                EventKind::SuspendCancelled,
                &w(false, 2, false)
            ),
            Some(State::LidOpen)
        );
    }

    #[test]
    fn timer_expired_with_live_request_suspends_a_lidless_desktop() {
        // The re-derive previously only accepted a lid-closed Countdown;
        // it must now accept a request-driven one, or a desktop's grace
        // timer fires and does nothing.
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &req(false)),
            Some(State::Suspending)
        );
    }

    #[test]
    fn stale_timer_with_withdrawn_request_rederives_not_suspends() {
        // Grace armed on a request that was then cancelled: the expiry
        // re-derives the world (lidless desktop -> LidOpen), never Suspending.
        assert_eq!(
            desired_state(
                State::Countdown,
                EventKind::TimerExpired,
                &w(false, 2, false)
            ),
            Some(State::LidOpen)
        );
    }

    #[test]
    fn sequence_request_then_cancel_within_grace_never_suspends() {
        // LidOpen -> request -> Countdown -> cancel -> Docked; a stale
        // TimerExpired must then find no request and stay out of Suspending.
        let end = drive(
            State::LidOpen,
            &[
                (
                    EventKind::SuspendRequested,
                    req(false),
                    Some(State::Countdown),
                ),
                (
                    EventKind::SuspendCancelled,
                    w(false, 2, false),
                    Some(State::LidOpen),
                ),
                (EventKind::TimerExpired, w(false, 2, false), None),
            ],
        );
        assert_eq!(end, State::LidOpen);
    }

    #[test]
    fn sequence_request_suspend_resume_cycle() {
        // The whole idle-suspend loop, with the request cleared before
        // Resumed (the daemon's contract).
        let end = drive(
            State::LidOpen,
            &[
                (
                    EventKind::SuspendRequested,
                    req(false),
                    Some(State::Countdown),
                ),
                (EventKind::TimerExpired, req(false), Some(State::Suspending)),
                (EventKind::Resumed, w(false, 2, false), Some(State::LidOpen)),
            ],
        );
        assert_eq!(end, State::LidOpen);
    }

    #[test]
    fn sequence_resumed_with_stale_request_reenters_countdown() {
        // If the daemon FAILS to clear the request before Resumed, the
        // machine re-enters Countdown - a self-perpetuating suspend loop.
        // This test pins WHY the dispatcher must clear it: the FSM alone
        // cannot, because Resumed re-derives world_state and the request
        // still reads true here.
        assert_eq!(
            desired_state(State::Suspending, EventKind::Resumed, &req(false)),
            Some(State::Countdown)
        );
    }

    #[test]
    fn sequence_inhibitor_toggle_during_idle_countdown() {
        // A media inhibitor appearing mid-countdown parks in Deferred and
        // resumes Countdown when it clears - the request stands throughout.
        let end = drive(
            State::LidOpen,
            &[
                (
                    EventKind::SuspendRequested,
                    req(false),
                    Some(State::Countdown),
                ),
                (EventKind::InhibitorOn, req(true), Some(State::Deferred)),
                (EventKind::InhibitorOff, req(false), Some(State::Countdown)),
            ],
        );
        assert_eq!(end, State::Countdown);
    }
}
