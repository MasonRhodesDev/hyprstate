//! Main lid/suspend FSM and the screen-DPMS sub-FSM.
//!
//! Port of hyprstate.py's `desired_state` / `_world_state` /
//! `desired_screen_state`. The functions are total over plain input structs;
//! the daemon snapshots its `Context` into `WorldInputs` / `ScreenInputs` at
//! dispatch time.

/// Main FSM states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Screen-DPMS sub-FSM states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    Active,
    DimPending,
    Dimmed,
}

impl ScreenState {
    pub fn as_str(self) -> &'static str {
        match self {
            ScreenState::Active => "SCREEN_ACTIVE",
            ScreenState::DimPending => "SCREEN_DIM_PENDING",
            ScreenState::Dimmed => "SCREEN_DIMMED",
        }
    }
}

/// Event kinds as the transition maps see them. The daemon's `Event` enum
/// carries payloads; it projects to this for the pure layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    ScreenTimerExpired,
    Resumed,
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
}

pub fn world_state(w: &WorldInputs) -> State {
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

/// Inputs of the screen-DPMS sub-FSM.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScreenInputs {
    pub locked: bool,
    pub inhibitor: bool,
}

/// Pure transition for the screen-DPMS sub-FSM.
///
/// Active only when the main FSM is showing a screen (LidOpen or Docked);
/// otherwise force Active. DimPending -> Dimmed is the one event-driven
/// transition; everything else is computed from (locked, inhibitor).
pub fn desired_screen_state(
    main: State,
    screen: ScreenState,
    ev: EventKind,
    s: &ScreenInputs,
) -> Option<ScreenState> {
    if !matches!(main, State::LidOpen | State::Docked) {
        return (screen != ScreenState::Active).then_some(ScreenState::Active);
    }

    if ev == EventKind::ScreenTimerExpired {
        return (screen == ScreenState::DimPending).then_some(ScreenState::Dimmed);
    }

    let target = if !(s.locked && s.inhibitor) {
        ScreenState::Active
    } else if screen == ScreenState::Dimmed {
        ScreenState::Dimmed // stay dimmed; only unlock/inhibit-off exits
    } else {
        ScreenState::DimPending
    };

    (target != screen).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(lid_closed: bool, ext_mon_count: u32, inhibitor: bool) -> WorldInputs {
        WorldInputs {
            lid_closed,
            ext_mon_count,
            inhibitor,
        }
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

    fn s(locked: bool, inhibitor: bool) -> ScreenInputs {
        ScreenInputs { locked, inhibitor }
    }

    #[test]
    fn test_screen_forced_active_when_no_screen_showing() {
        let inputs = s(true, true);
        assert_eq!(
            desired_screen_state(
                State::Countdown,
                ScreenState::Dimmed,
                EventKind::Reconcile,
                &inputs
            ),
            Some(ScreenState::Active)
        );
        assert_eq!(
            desired_screen_state(
                State::Countdown,
                ScreenState::Active,
                EventKind::Reconcile,
                &inputs
            ),
            None
        );
    }

    #[test]
    fn test_screen_dim_pending_when_locked_and_inhibited() {
        let inputs = s(true, true);
        assert_eq!(
            desired_screen_state(
                State::LidOpen,
                ScreenState::Active,
                EventKind::LockEngaged,
                &inputs
            ),
            Some(ScreenState::DimPending)
        );
    }

    #[test]
    fn test_screen_timer_dims_only_from_dim_pending() {
        let inputs = s(true, true);
        assert_eq!(
            desired_screen_state(
                State::Docked,
                ScreenState::DimPending,
                EventKind::ScreenTimerExpired,
                &inputs
            ),
            Some(ScreenState::Dimmed)
        );
        assert_eq!(
            desired_screen_state(
                State::Docked,
                ScreenState::Active,
                EventKind::ScreenTimerExpired,
                &inputs
            ),
            None
        );
    }

    #[test]
    fn test_screen_stays_dimmed_while_locked_and_inhibited() {
        let inputs = s(true, true);
        assert_eq!(
            desired_screen_state(
                State::LidOpen,
                ScreenState::Dimmed,
                EventKind::Reconcile,
                &inputs
            ),
            None
        );
    }

    #[test]
    fn test_screen_wakes_on_unlock_or_inhibitor_release() {
        assert_eq!(
            desired_screen_state(
                State::LidOpen,
                ScreenState::Dimmed,
                EventKind::LockReleased,
                &s(false, true)
            ),
            Some(ScreenState::Active)
        );
        assert_eq!(
            desired_screen_state(
                State::LidOpen,
                ScreenState::Dimmed,
                EventKind::InhibitorOff,
                &s(true, false)
            ),
            Some(ScreenState::Active)
        );
    }
}
