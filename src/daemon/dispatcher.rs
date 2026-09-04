//! The event loop: ctx updates, the exact v1 routing topology, and Layer-2
//! on_enter composition.
//!
//! Routing invariants ported from v1 (hyprstate.py dispatcher):
//! - RECONCILE (configreloaded) re-asserts the current states only — it
//!   never feeds desired_state — and ingests the active profile link (.active.lua) first.
//! - MONITORS_CHANGED: profile apply -> breadcrumb -> gpu drift -> dgpu
//!   runtime-PM pin -> power policy -> continue (never feeds the main FSM).
//! - gpu drift advice on AC/platform/gpu-override events happens in
//!   fall-through, NOT via RECONCILE; the dgpu pin rides the same gate.
//! - power policy fall-through gate: BatteryLowChanged flips,
//!   PowerOverrideChanged, PowerAcSettled.

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::ctx::Context;
use super::effectors::Effectors;
use super::event::{Event, ReconcileSnapshot};
use super::gpu_drift::{gpu_drift_check, resolve_session_gpu_mode};
use super::power_policy::power_policy_check;
use super::telemetry::TelemetryEmitter;
use crate::pure::fsm::{
    EventKind, State, StuckScreenInputs, desired_state, dpms_stuck_off, world_state,
};
use crate::pure::gpu::dgpu_runtime_pm_pinned;
use crate::pure::power::{battery_low_step, profile_from_platform_value};
use crate::pure::profiles::{EdpPolicy, GpuPref};
use crate::sysio::hyprctl;
use crate::sysio::profiles::load_profiles;
use crate::sysio::profiles::select_profile;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Fresh,
    Reassert,
}

async fn on_enter(state: State, entry: Entry, ctx: &mut Context, fx: &Effectors) -> bool {
    match state {
        State::LidOpen => {
            fx.cancel_grace_timer(ctx);
            fx.set_edp(true, ctx);
            true
        }
        State::Docked => {
            fx.cancel_grace_timer(ctx);
            fx.set_edp(false, ctx);
            true
        }
        State::Deferred => {
            fx.cancel_grace_timer(ctx);
            fx.set_edp(false, ctx);
            fx.pause_media();
            true
        }
        State::Countdown => {
            fx.set_edp(false, ctx);
            // A RECONCILE re-assert must NOT restart (extend) a live
            // countdown — v1 silently reset the 30s window on every
            // configreloaded.
            fx.start_grace_timer(ctx, entry == Entry::Fresh);
            true
        }
        State::Suspending => {
            fx.cancel_grace_timer(ctx);
            suspending_tail(ctx, fx).await
        }
    }
}

/// What decision 6 wants done, as a pure function of the power inputs.
/// Request only when genuinely discharging low: BOTH the raw and the
/// settled AC axis must be off - the raw one flips instantly at wake, so a
/// machine that wakes on a charger inside the 5 s settle window must not
/// self-request off a UPower percent event (review F2). Withdraw the
/// moment the reason is gone: raw AC back OR the battery recovered - a
/// plug-in during the 30 s grace must never ride to a suspend on the
/// charger (review F1); the effector scopes the withdraw to the daemon's
/// own "battery-low" file, so an idle-origin request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatteryLowAction {
    Request,
    Withdraw,
    Hold,
}

fn battery_low_action(
    low_battery: bool,
    on_ac: bool,
    on_ac_settled: bool,
    request_standing: bool,
) -> BatteryLowAction {
    if low_battery && !on_ac && !on_ac_settled && !request_standing {
        BatteryLowAction::Request
    } else if request_standing && (on_ac || !low_battery) {
        BatteryLowAction::Withdraw
    } else {
        BatteryLowAction::Hold
    }
}

/// Decision 6 (POWER_SPEC.md ladder): the daemon requests its own suspend
/// on low battery, and withdraws that request when the reason is gone. The
/// request rides the ordinary machinery - grace, lock proof, cancellation -
/// and `world_state` lets a battery-low request bypass the keep-awake claim
/// gate, so a paused video cannot ride the battery to zero. ctx is marked
/// standing only when the file was actually written (shadow and failed
/// writes stay consistent with what readers see, review F3), and cleared
/// only when the daemon's own file was actually removed - an idle-origin
/// request is not the daemon's to withdraw. Returns whether anything
/// changed.
pub(crate) fn battery_low_request_check(ctx: &mut Context, fx: &Effectors) -> bool {
    match battery_low_action(
        ctx.low_battery,
        ctx.on_ac,
        ctx.on_ac_settled,
        ctx.suspend_requested,
    ) {
        BatteryLowAction::Request => {
            if fx.request_suspend() {
                info!(
                    "battery-low: self-requesting suspend (locks first; 30 s grace is the plug-in window)"
                );
                ctx.suspend_requested = true;
                return true;
            }
            false
        }
        BatteryLowAction::Withdraw => {
            if fx.clear_battery_low_request() {
                ctx.suspend_requested = false;
                return true;
            }
            false
        }
        BatteryLowAction::Hold => false,
    }
}

/// Lock-before-suspend: proceed only after a live locker is proven.
/// A cached/stuck LockedHint is not proof; abort the same way as lock-timeout.
async fn suspending_tail(ctx: &mut Context, fx: &Effectors) -> bool {
    if fx.live_locker().await {
        info!("already locked; proceeding to suspend");
    } else {
        fx.request_lock().await;
        if fx.wait_for_lock(ctx).await && fx.live_locker().await {
            info!("lock engaged; proceeding to suspend");
        } else {
            warn!("lock did not engage in 2.0s — aborting suspend");
            return false;
        }
    }
    fx.do_suspend().await;
    true
}

fn log_state_transition(ctx: &Context, from: State, to: State, label: &str) {
    info!(
        "STATE: {} -> {} (event={label}, ext_mon={}, inhibitor={}, locked={}, on_ac={})",
        from.as_str(),
        to.as_str(),
        ctx.ext_mon_count,
        ctx.inhibitor(),
        ctx.locked,
        ctx.on_ac,
    );
}

/// Run both transition maps for one event kind; fire on_enter on change.
async fn evaluate_fsms(
    ctx: &mut Context,
    fx: &Effectors,
    kind: EventKind,
    label: &'static str,
    telem: &mut TelemetryEmitter,
) {
    let from = ctx.state;
    if let Some(new) = desired_state(ctx.state, kind, &ctx.world())
        && new != ctx.state
    {
        log_state_transition(ctx, ctx.state, new, label);
        let entered = if new == State::Suspending {
            if on_enter(new, Entry::Fresh, ctx, fx).await {
                ctx.state = new;
                true
            } else {
                // The timer transition is rejected when lock readiness is
                // unconfirmed. Stay in COUNTDOWN and re-arm a fresh grace
                // period so a later attempt can retry safely.
                fx.start_grace_timer(ctx, true);
                false
            }
        } else {
            ctx.state = new;
            on_enter(new, Entry::Fresh, ctx, fx).await
        };

        // Best-effort telemetry — never affects FSM behavior.
        telem.emit_help(
            ctx,
            "transition",
            label,
            from,
            ctx.state,
            if entered {
                on_enter_effector_names(new)
            } else {
                vec!["request_lock", "start_grace_timer"]
            },
        );
    } else {
        debug!(
            "ignored: {label} in {} (ext_mon={}, inhibitor={}, locked={}, on_ac={})",
            ctx.state.as_str(),
            ctx.ext_mon_count,
            ctx.inhibitor(),
            ctx.locked,
            ctx.on_ac,
        );
        // Ctx may still have changed (e.g. inhibitor while already LID_OPEN).
        telem.emit_help(ctx, "ctx", label, from, ctx.state, Vec::new());
    }
}

/// Map state to the effector names fired during on_enter (for telemetry).
fn on_enter_effector_names(state: State) -> Vec<&'static str> {
    match state {
        State::LidOpen => vec!["cancel_grace_timer", "set_edp_on"],
        State::Docked => vec!["cancel_grace_timer", "set_edp_off"],
        State::Deferred => vec!["cancel_grace_timer", "set_edp_off", "pause_media"],
        State::Countdown => vec!["set_edp_off", "start_grace_timer"],
        State::Suspending => vec!["cancel_grace_timer", "lock_then_suspend"],
    }
}

/// MONITORS_CHANGED branch: profile apply -> breadcrumb -> gpu drift ->
/// power policy. Never feeds the main FSM.
async fn handle_monitors_changed(ctx: &mut Context, fx: &Effectors, telem: &mut TelemetryEmitter) {
    let signature = hyprctl::monitor_signature().await;
    let profiles = load_profiles();
    let chosen = select_profile(&signature, &profiles);
    match chosen {
        None => {
            let mut sorted = signature.clone();
            sorted.sort();
            info!(
                "PROFILE: no match for signature={sorted:?} (have {} profiles)",
                profiles.len()
            );
            ctx.active_profile_rev = None;
        }
        Some(p) => fx.apply_profile(p, ctx),
    }
    // Breadcrumb before drift check: "relog to apply" must be satisfiable
    // by one relog, so next-login select needs the same profile overlay the
    // drift computation is about to use.
    let gpu_pref = chosen.map(|p| p.gpu).unwrap_or(GpuPref::Auto);
    fx.write_gpu_breadcrumb(chosen.map(|p| p.gpu));
    let mode = gpu_drift_check(ctx, fx, "monitors changed", gpu_pref);
    fx.sync_dgpu_pin(ctx, dgpu_runtime_pm_pinned(mode)).await;
    // Docked-ness (ext_mon_count) is a power-policy input.
    power_policy_check(ctx, fx).await;

    // A dock topology change is both when Hyprland strands eDP workspaces
    // (disable with no enabled backup during an undock flap) and when they
    // become re-homeable (an external is back). Hyprland never re-homes them
    // itself once stranded, so repair here whenever the eDP is meant to be
    // off and an external exists to receive them.
    let edp_should_be_off = match ctx.edp_policy {
        EdpPolicy::Disable => true,
        EdpPolicy::Enable => false,
        EdpPolicy::Auto => ctx.state != State::LidOpen,
    };
    if edp_should_be_off && ctx.ext_mon_count > 0 {
        fx.rehome_edp_workspaces();
    }
    telem.emit_help(
        ctx,
        "snapshot",
        "MonitorsChanged",
        ctx.state,
        ctx.state,
        Vec::new(),
    );
}

/// Diff a reconciler snapshot against ctx; repair, route repairs back into
/// the machines, and re-assert the eDP/DPMS invariants.
async fn handle_reconcile_tick(
    snap: ReconcileSnapshot,
    ctx: &mut Context,
    fx: &Effectors,
    telem: &mut TelemetryEmitter,
) {
    let mut drift: Vec<String> = Vec::new();
    let mut fsm_drift = false;
    let mut power_drift = false;

    if ctx.lid_present && snap.lid_closed != ctx.lid_closed {
        drift.push(format!(
            "lid_closed {}->{}",
            ctx.lid_closed, snap.lid_closed
        ));
        ctx.lid_closed = snap.lid_closed;
        fsm_drift = true;
    } else if !ctx.lid_present && snap.lid_closed {
        // A closed lid reported on a machine declared lidless: log it, but
        // never let it drive the FSM (that is the whole point of absent).
        warn!("reconciler: lid reported closed but power.conf declares lid absent — ignoring");
    }
    // Re-read the file HERE, not from the snapshot: this handler runs after
    // the Resumed arm (events are processed serially), so a snapshot taken
    // before Resumed cleared the request cannot resurrect it. Existence is
    // the signal, matching the poller and the TimerExpired guard.
    let request_now = crate::paths::suspend_request_standing();
    if request_now != ctx.suspend_requested {
        drift.push(format!(
            "suspend_requested {}->{}",
            ctx.suspend_requested, request_now
        ));
        ctx.suspend_requested = request_now;
        fsm_drift = true;
    }
    if snap.ext_mon_count != ctx.ext_mon_count {
        drift.push(format!(
            "ext_mon {}->{}",
            ctx.ext_mon_count, snap.ext_mon_count
        ));
        ctx.ext_mon_count = snap.ext_mon_count;
        fsm_drift = true;
        power_drift = true;
        // Monitor events were evidently missed, so profile reconciliation
        // was missed too — re-derive via the normal debounced path.
        fx.schedule_profile_reconcile(ctx);
    }
    if snap.logind_inhibitor != ctx.logind_inhibitor {
        drift.push(format!(
            "logind_inh {}->{}",
            ctx.logind_inhibitor, snap.logind_inhibitor
        ));
        ctx.logind_inhibitor = snap.logind_inhibitor;
        fsm_drift = true;
    }
    if snap.wayland_inhibitor != ctx.wayland_inhibitor {
        drift.push(format!(
            "wayland_inh {}->{}",
            ctx.wayland_inhibitor, snap.wayland_inhibitor
        ));
        ctx.wayland_inhibitor = snap.wayland_inhibitor;
        fsm_drift = true;
    }
    if let Some(locked) = snap.locked
        && locked != ctx.locked
    {
        drift.push(format!("locked {}->{locked} (LockedHint)", ctx.locked));
        ctx.locked = locked;
        // Mirror into the watch channel: wait_for_lock reads it, and
        // lock_watcher only sends on signal edges — which we evidently
        // missed.
        let _ = fx.locked_tx.send(locked);
        fsm_drift = true;
    }
    if let Some(on_ac) = snap.on_ac
        && on_ac != ctx.on_ac
    {
        drift.push(format!("on_ac {}->{on_ac} (sysfs fallback)", ctx.on_ac));
        ctx.on_ac = on_ac;
        power_drift = true;
    }

    if !drift.is_empty() {
        warn!("reconciler ctx drift: {}", drift.join("; "));
    }

    // Repaired power inputs must reach power policy — covers
    // boot-on-battery with UPower down.
    if power_drift {
        ctx.on_ac_settled = ctx.on_ac;
        power_policy_check(ctx, fx).await;
        // Repaired power inputs feed decision 6 like live ones - covers
        // boot-on-battery with UPower down (review F6).
        battery_low_request_check(ctx, fx);
    }
    // Repaired FSM inputs must DRIVE the machines, not just describe them.
    if fsm_drift {
        evaluate_fsms(ctx, fx, EventKind::CtxRepaired, "CtxRepaired", telem).await;
    }

    if ctx.state == State::Suspending {
        return;
    }

    // Ingest any out-of-band .active.lua repoint before enforcing the eDP
    // invariant — enforcing a stale edp_policy would fight a manual
    // `profile switch` every pass.
    fx.ingest_active_profile(ctx);

    // eDP invariant: the resolved policy (profile override or lid-driven
    // default) vs reality.
    let should_be_enabled = match ctx.edp_policy {
        EdpPolicy::Disable => false,
        EdpPolicy::Enable => true,
        EdpPolicy::Auto => ctx.state == State::LidOpen,
    };
    if let Some(disabled) = snap.edp_disabled {
        if should_be_enabled && disabled {
            warn!(
                "reconciler: state={} edp_policy={} but eDP disabled — re-enabling",
                ctx.state.as_str(),
                ctx.edp_policy.as_str()
            );
            fx.set_edp(true, ctx);
        } else if !should_be_enabled && !disabled {
            warn!(
                "reconciler: state={} edp_policy={} but eDP enabled — re-disabling",
                ctx.state.as_str(),
                ctx.edp_policy.as_str()
            );
            fx.set_edp(false, ctx);
        }
    }

    // STUCK-DPMS backstop: hypridle can lose its own wake (see
    // `dpms_stuck_off`), leaving a live session driving dark panels that no
    // input will recover. Repair only when the blank is provably unowned.
    let cursor_moved = match (ctx.last_cursor_pos, snap.cursor_pos) {
        (Some(prev), Some(now)) => prev != now,
        // First sample (or hyprctl failed) proves nothing either way.
        _ => false,
    };
    // The cursor is only sampled while something is dark, so drop the
    // baseline as soon as it is not. Keeping it would let a position left
    // over from an earlier dark episode read as movement on the first tick
    // of the next one and undo an ordinary idle blank immediately.
    ctx.last_cursor_pos = snap.cursor_pos;
    if let Some(dpms_off) = snap.dpms_off {
        let inputs = StuckScreenInputs {
            dpms_off,
            locked: ctx.locked,
            cursor_moved,
        };
        if dpms_stuck_off(ctx.state, &inputs) {
            warn!(
                "reconciler: state={} but an enabled output is DPMS off \
                 (locked={}, cursor_moved={}) — re-asserting dpms on \
                 (hypridle dropped its on-resume?)",
                ctx.state.as_str(),
                ctx.locked,
                cursor_moved,
            );
            fx.dpms_on();
        }
    }

    telem.emit_help(
        ctx,
        "snapshot",
        "ReconcileTick",
        ctx.state,
        ctx.state,
        Vec::new(),
    );
}

pub async fn run(mut rx: mpsc::Receiver<Event>, mut ctx: Context, fx: Effectors) {
    let mut telem = TelemetryEmitter::new();
    ctx.state = world_state(&ctx.world());
    info!(
        "initial state: {} (ext_mon={}, inhibitor={}, locked={}, on_ac={})",
        ctx.state.as_str(),
        ctx.ext_mon_count,
        ctx.inhibitor(),
        ctx.locked,
        ctx.on_ac,
    );
    let _ = on_enter(ctx.state, Entry::Fresh, &mut ctx, &fx).await;
    telem.emit_help(
        &ctx,
        "snapshot",
        "Startup",
        ctx.state,
        ctx.state,
        on_enter_effector_names(ctx.state),
    );

    // Initial dgpu runtime-PM pin. dgpu mode must block D3cold from login
    // onward — the FW16 DCN wedge bites during the no-display-yet window
    // before the compositor brings the dGPU's output up, so we can't wait for
    // the first MONITORS_CHANGED. Independent of gpu_actual (mode resolves
    // from the override/profile/platform inputs alone).
    {
        let signature = hyprctl::monitor_signature().await;
        let profiles = load_profiles();
        let gpu_pref = select_profile(&signature, &profiles)
            .map(|p| p.gpu)
            .unwrap_or(GpuPref::Auto);
        let (mode, _) = resolve_session_gpu_mode(gpu_pref);
        fx.sync_dgpu_pin(&mut ctx, dgpu_runtime_pm_pinned(mode))
            .await;
    }

    while let Some(ev) = rx.recv().await {
        let kind = ev.kind();
        let label = ev.label();

        // ---- branches that never reach the FSMs ----
        match ev {
            Event::ConfigReloaded => {
                if ctx.state != State::Suspending {
                    // `profile switch` repoints .active.conf and reloads;
                    // ingest BEFORE re-asserting so set_edp uses the new
                    // profile's policy.
                    fx.ingest_active_profile(&mut ctx);
                    info!(
                        "RECONCILE (configreloaded): re-asserting {}",
                        ctx.state.as_str()
                    );
                    let _ = on_enter(ctx.state, Entry::Reassert, &mut ctx, &fx).await;
                }
                continue;
            }
            Event::ReconcileTick(snap) => {
                handle_reconcile_tick(*snap, &mut ctx, &fx, &mut telem).await;
                continue;
            }
            Event::MonitorsChanged => {
                handle_monitors_changed(&mut ctx, &fx, &mut telem).await;
                continue; // profile reconciliation does not feed the main FSM
            }
            Event::ProfilesChanged => {
                info!("PROFILE: source directory changed — re-resolving");
                fx.schedule_profile_reconcile(&mut ctx);
                continue;
            }

            // ---- ctx updates that fall through to the FSMs ----
            Event::Lid(closed) => {
                if ctx.lid_present {
                    ctx.lid_closed = closed;
                } else {
                    warn!(
                        "lid event on a machine declared lidless (power.conf lid=absent) — ignoring"
                    );
                    continue;
                }
            }
            Event::MonitorHotplug { added, ref name } => {
                debug!(
                    "monitor {}: {name}",
                    if added { "added" } else { "removed" }
                );
                ctx.ext_mon_count = hyprctl::ext_monitor_count(ctx.ext_mon_count).await;
                // Coalesce mode/scale negotiation bursts before profile
                // reconciliation.
                fx.schedule_profile_reconcile(&mut ctx);
            }
            Event::Inhibitor { wayland, active } => {
                if wayland {
                    ctx.wayland_inhibitor = active;
                } else {
                    ctx.logind_inhibitor = active;
                }
            }
            Event::LockChanged(locked) => ctx.locked = locked,
            Event::AcChanged(on_ac) => {
                ctx.on_ac = on_ac;
                info!("AC: {label} (on_ac={on_ac})");
                // Raw plug-in withdraws a battery-low request NOW, not
                // after the 5 s settle: a suspend on the charger is the
                // worse failure by far, so cancellation follows the fast
                // signal while requests wait for both axes.
                battery_low_request_check(&mut ctx, &fx);
                fx.schedule_power_settle(&mut ctx);
            }
            Event::PowerAcSettled => {
                ctx.on_ac_settled = ctx.on_ac;
                // An unplug that settles while already low must request
                // (decision 6), and a plug-in that settles must withdraw a
                // standing battery-low request - re-derivation alone never
                // cancels one, because suspend_requested short-circuits
                // world_state (review F1).
                battery_low_request_check(&mut ctx, &fx);
            }
            Event::PowerOverrideChanged(ref word) => {
                // Echoes of the daemon's own writes arrive with ctx already
                // matching — those land as no-ops by design.
                match word.as_deref() {
                    None => {
                        ctx.power_override = None;
                        ctx.power_override_base = None;
                    }
                    Some(w) => match w.parse::<crate::pure::power::PowerProfile>() {
                        Ok(p) if ctx.power_override == Some(p) => {} // own echo
                        Ok(p) => {
                            ctx.power_override = Some(p);
                            ctx.power_override_base = None; // stamped at next check
                        }
                        Err(_) => warn!("power-override: unknown profile {w:?} — ignoring"),
                    },
                }
            }
            Event::BatteryPercent(pct) => {
                ctx.battery_percent = Some(pct);
                let new_low = battery_low_step(ctx.low_battery, pct, ctx.battery_low_pct);
                let flipped = new_low != ctx.low_battery;
                ctx.low_battery = new_low;
                // Decided model, decision 6: battery-low requests and
                // withdrawals ride every percent event, not only the flip,
                // so a machine that woke still-low re-requests (the 30 s
                // grace is the plug-in window) and a recovery withdraws.
                let changed = battery_low_request_check(&mut ctx, &fx);
                if !flipped && !changed {
                    continue; // no flip -> no event existed in v1
                }
            }
            Event::PlatformProfileChanged(ref value) => {
                // Self-writes (or anything inside the suppression window of
                // an apply) are ignored; external writes are adopted as a
                // manual override — never reverted. Either way the event
                // still falls through to the gpu-drift gate, as in v1.
                if ctx
                    .self_writes
                    .is_self_write(std::time::Instant::now(), value.as_deref())
                {
                    debug!("platform_profile -> {value:?}: own write");
                } else {
                    let profile = profile_from_platform_value(value.as_deref());
                    fx.adopt_power_override(&mut ctx, profile);
                    power_policy_check(&mut ctx, &fx).await;
                }
            }
            Event::GpuOverrideChanged(ref word) => {
                debug!("gpu-select override -> {word:?}");
            }
            Event::PowerdAppeared => {
                if !ctx.powerd_available {
                    info!("powerd appeared on the bus — re-enabling power applies");
                    ctx.powerd_available = true;
                    ctx.powerd_warned = false;
                    ctx.power_applied = None; // force a re-apply
                    ctx.dgpu_pinned = None; // re-push the dgpu pin too
                }
                ctx.on_ac_settled = ctx.on_ac;
            }
            Event::SuspendRequestChanged(word) => {
                let requested = word.is_some();
                if requested != ctx.suspend_requested {
                    info!(
                        "idle-suspend request {}",
                        if requested { "standing" } else { "withdrawn" }
                    );
                }
                ctx.suspend_requested = requested;
            }
            Event::TimerExpired => {
                // Close the cancel-vs-expiry race: hypridle's on-resume
                // deletes the request file on first input, but the poller
                // echoes that at 2 s cadence and the grace timer can fire
                // inside the gap. The file is the authority; a request the
                // user just withdrew must not suspend the machine.
                if ctx.suspend_requested && !crate::paths::suspend_request_standing() {
                    info!("idle-suspend request withdrawn at grace expiry");
                    ctx.suspend_requested = false;
                }
            }
            Event::Resumed => {
                // Clear BEFORE evaluate_fsms re-derives: Resumed maps
                // Suspending back to world_state, and a stale standing
                // request would re-enter Countdown - a wake that schedules
                // its own next suspend, looping the machine. desktop-commons
                // adds a conformance assertion pinning this ordering in the
                // registry PR that follows.
                ctx.suspend_requested = false;
                fx.clear_suspend_request();
            }
        }

        // Re-arm the brightness takeover guard on resume: panel raw values
        // drift across suspend and must not read as a user adjustment.
        if kind == EventKind::Resumed {
            fx.brightness_rearm(&mut ctx);
        }

        // GPU drift advice on power/override changes (NOT via RECONCILE).
        if matches!(
            kind,
            EventKind::AcPlugged
                | EventKind::AcUnplugged
                | EventKind::PlatformProfileChanged
                | EventKind::GpuOverrideChanged
        ) {
            let signature = hyprctl::monitor_signature().await;
            let profiles = load_profiles();
            let chosen = select_profile(&signature, &profiles);
            let gpu_pref = chosen.map(|p| p.gpu).unwrap_or(GpuPref::Auto);
            let mode = gpu_drift_check(&mut ctx, &fx, label, gpu_pref);
            fx.sync_dgpu_pin(&mut ctx, dgpu_runtime_pm_pinned(mode))
                .await;
        }

        // Power policy evaluation on its (debounced/derived) inputs.
        if matches!(
            kind,
            EventKind::BatteryLowChanged
                | EventKind::PowerOverrideChanged
                | EventKind::PowerAcSettled
        ) {
            power_policy_check(&mut ctx, &fx).await;
            telem.emit_help(&ctx, "ctx", label, ctx.state, ctx.state, Vec::new());
        }

        evaluate_fsms(&mut ctx, &fx, kind, label, &mut telem).await;
    }
}

#[cfg(test)]
mod battery_low_tests {
    use super::{BatteryLowAction, battery_low_action};

    #[test]
    fn requests_only_when_genuinely_discharging_low() {
        // (low, on_ac, on_ac_settled, standing) -> action
        assert_eq!(
            battery_low_action(true, false, false, false),
            BatteryLowAction::Request
        );
        // Review F2: a machine that wakes on the charger has raw on_ac
        // true while on_ac_settled is still false for the 5 s debounce; a
        // UPower percent event in that window must NOT request.
        assert_eq!(
            battery_low_action(true, true, false, false),
            BatteryLowAction::Hold
        );
        // Stale-true settled axis alone also blocks a request.
        assert_eq!(
            battery_low_action(true, false, true, false),
            BatteryLowAction::Hold
        );
        // Not low, nothing standing: nothing to do.
        assert_eq!(
            battery_low_action(false, false, false, false),
            BatteryLowAction::Hold
        );
    }

    #[test]
    fn withdraws_the_moment_the_reason_is_gone() {
        // Review F1: a plug-in during the 30 s grace must cancel the
        // standing request - re-derivation alone cannot, because
        // suspend_requested short-circuits world_state. The raw axis is
        // enough: cancellation follows the fast signal.
        assert_eq!(
            battery_low_action(true, true, false, true),
            BatteryLowAction::Withdraw
        );
        // Battery recovered (charged above threshold + hysteresis).
        assert_eq!(
            battery_low_action(false, false, false, true),
            BatteryLowAction::Withdraw
        );
        // Still discharging low with a request standing: hold, idempotent.
        assert_eq!(
            battery_low_action(true, false, false, true),
            BatteryLowAction::Hold
        );
    }
}
