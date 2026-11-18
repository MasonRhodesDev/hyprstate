#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum State {
    LidOpen,
    Docked,
    Deferred,
    Countdown,
    Suspending,
}

impl State {
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
    SuspendRequested,
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

#[derive(Debug, Clone, Copy, Default)]
pub struct WorldInputs {
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub inhibitor: bool,
    pub suspend_requested: bool,
    pub locked: bool,
    pub battery_low: bool,
    pub lid_ac_hold: bool,
    pub sleep_block: bool,
}

pub fn claim_governs(w: &WorldInputs) -> bool {
    w.inhibitor && !w.locked && !w.battery_low
}

pub fn sleep_block_governs(w: &WorldInputs) -> bool {
    w.suspend_requested && w.sleep_block && !w.battery_low
}

pub fn world_state(w: &WorldInputs) -> State {
    let claim = claim_governs(w);
    if w.suspend_requested {
        return if claim || sleep_block_governs(w) {
            State::Deferred
        } else {
            State::Countdown
        };
    }
    if w.lid_closed && w.ext_mon_count >= 1 {
        return State::Docked;
    }
    if !w.lid_closed {
        State::LidOpen
    } else if claim || w.lid_ac_hold {
        State::Deferred
    } else {
        State::Countdown
    }
}

pub fn desired_state(state: State, ev: EventKind, w: &WorldInputs) -> Option<State> {
    if ev == EventKind::TimerExpired {
        if state != State::Countdown {
            return None;
        }
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

#[derive(Debug, Clone, Copy, Default)]
pub struct StuckScreenInputs {
    pub dpms_off: bool,
    pub locked: bool,
    pub cursor_moved: bool,
}

pub fn dpms_stuck_off(main: State, s: &StuckScreenInputs) -> bool {
    if !s.dpms_off {
        return false;
    }
    if !matches!(main, State::LidOpen | State::Docked) {
        return false;
    }
    !s.locked || s.cursor_moved
}

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
            ..WorldInputs::default()
        }
    }

    fn req(inhibitor: bool) -> WorldInputs {
        WorldInputs {
            lid_closed: false,
            ext_mon_count: 2,
            inhibitor,
            suspend_requested: true,
            ..WorldInputs::default()
        }
    }

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
        assert!(!dpms_stuck_off(State::Docked, &stuck(true, true, false)));
    }

    #[test]
    fn test_stuck_dpms_repairs_locked_session_once_user_returns() {
        assert!(dpms_stuck_off(State::Docked, &stuck(true, true, true)));
    }

    #[test]
    fn test_stuck_dpms_repairs_unlocked_dark_session_without_cursor_proof() {
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

    fn locked_req(sleep_block: bool) -> WorldInputs {
        WorldInputs {
            locked: true,
            sleep_block,
            ..req(false)
        }
    }

    #[test]
    fn sleep_block_defers_a_locked_idle_suspend() {
        assert_eq!(world_state(&locked_req(false)), State::Countdown);
        assert_eq!(world_state(&locked_req(true)), State::Deferred);
    }

    #[test]
    fn sleep_block_release_resumes_the_countdown() {
        drive(
            State::LidOpen,
            &[
                (
                    EventKind::SuspendRequested,
                    locked_req(true),
                    Some(State::Deferred),
                ),
                (
                    EventKind::InhibitorOff,
                    locked_req(false),
                    Some(State::Countdown),
                ),
                (
                    EventKind::InhibitorOn,
                    locked_req(true),
                    Some(State::Deferred),
                ),
            ],
        );
    }

    #[test]
    fn sleep_block_taken_during_grace_stops_the_suspend() {
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &locked_req(true)),
            Some(State::Deferred)
        );
        assert_eq!(
            desired_state(
                State::Countdown,
                EventKind::TimerExpired,
                &locked_req(false)
            ),
            Some(State::Suspending)
        );
    }

    #[test]
    fn sleep_block_yields_to_low_battery() {
        let w = WorldInputs {
            battery_low: true,
            ..locked_req(true)
        };
        assert!(!sleep_block_governs(&w));
        assert_eq!(world_state(&w), State::Countdown);
    }

    #[test]
    fn sleep_block_leaves_the_lid_chain_alone() {
        let w = WorldInputs {
            lid_closed: true,
            sleep_block: true,
            ..WorldInputs::default()
        };
        assert!(!sleep_block_governs(&w));
        assert_eq!(world_state(&w), State::Countdown);
    }

    #[test]
    fn suspend_request_wins_over_lid_chain() {
        assert_eq!(world_state(&req(false)), State::Countdown);
    }

    #[test]
    fn a_docked_laptop_with_a_request_suspends() {
        let docked_request = WorldInputs {
            lid_closed: true,
            ext_mon_count: 2,
            inhibitor: false,
            suspend_requested: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&docked_request), State::Countdown);
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &docked_request),
            Some(State::Suspending),
            "an idle docked laptop rides the request to suspend"
        );
    }

    #[test]
    fn suspend_request_with_inhibitor_is_deferred() {
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
        assert_eq!(
            desired_state(State::Countdown, EventKind::TimerExpired, &req(false)),
            Some(State::Suspending)
        );
    }

    #[test]
    fn stale_timer_with_withdrawn_request_rederives_not_suspends() {
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
        assert_eq!(
            desired_state(State::Suspending, EventKind::Resumed, &req(false)),
            Some(State::Countdown)
        );
    }

    #[test]
    fn sequence_inhibitor_toggle_during_idle_countdown() {
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

    #[test]
    fn lock_ends_a_claims_authority() {
        let world = WorldInputs {
            inhibitor: true,
            suspend_requested: true,
            locked: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&world), State::Countdown);
        let unlocked = WorldInputs {
            locked: false,
            ..world
        };
        assert_eq!(world_state(&unlocked), State::Deferred);
    }

    #[test]
    fn battery_low_overrides_the_claim() {
        let world = WorldInputs {
            inhibitor: true,
            suspend_requested: true,
            battery_low: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&world), State::Countdown);
    }

    #[test]
    fn a_request_outranks_docked() {
        let idle_docked = WorldInputs {
            lid_closed: true,
            ext_mon_count: 2,
            suspend_requested: true,
            locked: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&idle_docked), State::Countdown);
        assert_eq!(world_state(&w(true, 2, false)), State::Docked);
    }

    #[test]
    fn a_docked_manual_request_under_a_claim_defers() {
        let docked_claimed = WorldInputs {
            lid_closed: true,
            ext_mon_count: 2,
            inhibitor: true,
            suspend_requested: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&docked_claimed), State::Deferred);
    }

    #[test]
    fn lid_close_mid_call_still_defers_until_locked() {
        let mid_call = WorldInputs {
            lid_closed: true,
            inhibitor: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&mid_call), State::Deferred);
        let locked = WorldInputs {
            locked: true,
            ..mid_call
        };
        assert_eq!(world_state(&locked), State::Countdown);
    }

    #[test]
    fn lid_ac_hold_defers_the_lid_until_unplugged() {
        let on_charger = WorldInputs {
            lid_closed: true,
            lid_ac_hold: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&on_charger), State::Deferred);
        let unplugged = WorldInputs {
            lid_ac_hold: false,
            ..on_charger
        };
        drive(
            State::Deferred,
            &[
                (EventKind::AcUnplugged, unplugged, Some(State::Countdown)),
                (EventKind::TimerExpired, unplugged, Some(State::Suspending)),
            ],
        );
    }

    #[test]
    fn lid_ac_hold_is_not_a_claim() {
        let held = WorldInputs {
            lid_closed: true,
            lid_ac_hold: true,
            locked: true,
            ..WorldInputs::default()
        };
        assert_eq!(world_state(&held), State::Deferred);
        let requested = WorldInputs {
            suspend_requested: true,
            ..held
        };
        assert_eq!(world_state(&requested), State::Countdown);
        let docked = WorldInputs {
            ext_mon_count: 1,
            ..held
        };
        assert_eq!(world_state(&docked), State::Docked);
        assert!(!claim_governs(&held), "the hold must not read as a claim");
    }
}
