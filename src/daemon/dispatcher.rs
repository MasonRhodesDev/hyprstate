use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::ctx::Context;
use super::effectors::{Effectors, SuspendOutcome, edp_must_be_on};
use super::event::{Event, ReconcileSnapshot};
use super::gpu_drift::{gpu_drift_check, resolve_session_gpu_mode};
use super::kbd_policy::kbd_policy_check;
use super::power_policy::power_policy_check;
use super::sources::ReconcileTrigger;
use super::telemetry::TelemetryEmitter;
use crate::pure::fsm::{
    EventKind, State, StuckScreenInputs, claim_governs, desired_state, dpms_stuck_off, world_state,
};
use crate::pure::gpu::dgpu_runtime_pm_pinned;
use crate::pure::kbd::DEFAULT_KBD_LEVEL_PCT;
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
    if state == State::Deferred && ctx.wayland_inhibitor {
        fx.start_claim_watch(ctx);
    } else {
        fx.cancel_claim_watch(ctx);
    }
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
            if claim_governs(&ctx.world()) {
                fx.pause_media();
            }
            true
        }
        State::Countdown => {
            fx.set_edp(false, ctx);
            fx.start_grace_timer(ctx, entry == Entry::Fresh);
            true
        }
        State::Suspending => {
            fx.cancel_grace_timer(ctx);
            suspending_tail(ctx, fx).await
        }
    }
}

async fn refresh_wayland_claim(ctx: &mut Context) {
    let Some(held) = hyprctl::wayland_idle_inhibited().await else {
        return;
    };
    if held != ctx.wayland_inhibitor {
        info!(
            "wayland idle-inhibit claim {} (sampled)",
            if held { "held" } else { "released" }
        );
        ctx.wayland_inhibitor = held;
    }
}

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
    match fx.do_suspend(ctx.world()).await {
        SuspendOutcome::Suspended => true,
        SuspendOutcome::SleepBlocked => {
            ctx.sleep_block = true;
            false
        }
        SuspendOutcome::Failed => {
            warn!("suspend refused - staying in COUNTDOWN to retry");
            false
        }
    }
}

fn log_state_transition(ctx: &Context, from: State, to: State, label: &str) {
    info!(
        "STATE: {} -> {} (event={label}, ext_mon={}, inhibitor={}, sleep_block={}, locked={}, on_ac={})",
        from.as_str(),
        to.as_str(),
        ctx.ext_mon_count,
        ctx.inhibitor(),
        ctx.sleep_block,
        ctx.locked,
        ctx.on_ac,
    );
    tracing::event!(
        name: "state.transition",
        target: "hyprstate.trace",
        tracing::Level::INFO,
        from = from.as_str(),
        to = to.as_str(),
        cause = label,
        ext_mon = ctx.ext_mon_count,
        inhibitor = ctx.inhibitor(),
        sleep_block = ctx.sleep_block,
        locked = ctx.locked,
        on_ac = ctx.on_ac,
        suspend_requested = ctx.suspend_requested,
        lid_ac_hold = ctx.world().lid_ac_hold,
    );
}

fn enter_state_span(ctx: &mut Context, to: State) {
    ctx.state_span = Some(tracing::info_span!(
        target: "hyprstate.trace",
        "state.occupancy",
        state = to.as_str()
    ));
}

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
            } else if world_state(&ctx.world()) == State::Deferred {
                log_state_transition(ctx, ctx.state, State::Deferred, label);
                ctx.state = State::Deferred;
                on_enter(State::Deferred, Entry::Fresh, ctx, fx).await
            } else {
                fx.start_grace_timer(ctx, true);
                false
            }
        } else {
            ctx.state = new;
            on_enter(new, Entry::Fresh, ctx, fx).await
        };

        if entered {
            let settled = ctx.state;
            enter_state_span(ctx, settled);
        }

        telem.emit_help(
            ctx,
            "transition",
            label,
            from,
            ctx.state,
            if entered {
                on_enter_effector_names(ctx.state, ctx)
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
        telem.emit_help(ctx, "ctx", label, from, ctx.state, Vec::new());
    }
}

fn on_enter_effector_names(state: State, ctx: &Context) -> Vec<&'static str> {
    match state {
        State::LidOpen => vec!["cancel_grace_timer", "set_edp_on"],
        State::Docked => vec!["cancel_grace_timer", "set_edp_off"],
        State::Deferred if claim_governs(&ctx.world()) => {
            vec!["cancel_grace_timer", "set_edp_off", "pause_media"]
        }
        State::Deferred => vec!["cancel_grace_timer", "set_edp_off"],
        State::Countdown => vec!["set_edp_off", "start_grace_timer"],
        State::Suspending => vec!["cancel_grace_timer", "lock_then_suspend"],
    }
}

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
    let gpu_pref = chosen.map(|p| p.gpu).unwrap_or(GpuPref::Auto);
    fx.write_gpu_breadcrumb(chosen.map(|p| p.gpu));
    let mode = gpu_drift_check(ctx, fx, "monitors changed", gpu_pref);
    fx.sync_dgpu_pin(ctx, dgpu_runtime_pm_pinned(mode)).await;
    power_policy_check(ctx, fx).await;

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
        warn!("reconciler: lid reported closed but power.conf declares lid absent — ignoring");
    }
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
    if snap.sleep_block != ctx.sleep_block {
        drift.push(format!(
            "sleep_block {}->{}",
            ctx.sleep_block, snap.sleep_block
        ));
        ctx.sleep_block = snap.sleep_block;
        fsm_drift = true;
    }
    if let Some(wayland) = snap.wayland_inhibitor
        && wayland != ctx.wayland_inhibitor
    {
        drift.push(format!("wayland_inh {}->{wayland}", ctx.wayland_inhibitor));
        ctx.wayland_inhibitor = wayland;
        fsm_drift = true;
    }
    if let Some(locked) = snap.locked
        && locked != ctx.locked
    {
        drift.push(format!("locked {}->{locked} (LockedHint)", ctx.locked));
        ctx.locked = locked;
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
        warn!(
            "reconciler ctx drift ({}): {}",
            snap.reasons,
            drift.join("; ")
        );
    }

    if power_drift {
        ctx.on_ac_settled = ctx.on_ac;
        power_policy_check(ctx, fx).await;
        battery_low_request_check(ctx, fx);
    }
    if fsm_drift {
        evaluate_fsms(ctx, fx, EventKind::CtxRepaired, "CtxRepaired", telem).await;
    }

    if ctx.state == State::Suspending {
        return;
    }

    fx.ingest_active_profile(ctx);

    let should_be_enabled = match ctx.edp_policy {
        EdpPolicy::Disable => false,
        EdpPolicy::Enable => true,
        EdpPolicy::Auto => ctx.state == State::LidOpen,
    };
    let should_be_enabled = edp_must_be_on(should_be_enabled, ctx.ext_mon_count);
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

    let cursor_moved = match (ctx.last_cursor_pos, snap.cursor_pos) {
        (Some(prev), Some(now)) => prev != now,
        _ => false,
    };
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
    let initial = ctx.state;
    enter_state_span(&mut ctx, initial);
    info!(
        "initial state: {} (ext_mon={}, inhibitor={}, sleep_block={}, locked={}, on_ac={})",
        ctx.state.as_str(),
        ctx.ext_mon_count,
        ctx.inhibitor(),
        ctx.sleep_block,
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
        on_enter_effector_names(ctx.state, &ctx),
    );

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

    fx.sync_waybar(&mut ctx);
    kbd_policy_check(&mut ctx, &fx, true).await;
    fx.request_reconcile(ReconcileTrigger::Startup);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let ev = tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => ev,
                None => break,
            },
            name = &mut shutdown => {
                info!("{name}: shutting down in {}", ctx.state.as_str());
                break;
            }
        };
        dispatch(ev, &mut ctx, &fx, &mut telem).await;
        fx.sync_waybar(&mut ctx);
    }
    drop(ctx);
}

async fn shutdown_signal() -> &'static str {
    let hook = |name: &'static str, kind: SignalKind| match signal(kind) {
        Ok(stream) => Some(stream),
        Err(e) => {
            warn!(
                "{name} handler unavailable ({e}); {name} will end the daemon without a clean exit"
            );
            None
        }
    };
    let mut term = hook("SIGTERM", SignalKind::terminate());
    let mut int = hook("SIGINT", SignalKind::interrupt());
    let mut hup = hook("SIGHUP", SignalKind::hangup());
    tokio::select! {
        _ = recv_or_pending(term.as_mut()) => "SIGTERM",
        _ = recv_or_pending(int.as_mut()) => "SIGINT",
        _ = recv_or_pending(hup.as_mut()) => "SIGHUP",
    }
}

async fn recv_or_pending(sig: Option<&mut Signal>) {
    if let Some(sig) = sig
        && sig.recv().await.is_some()
    {
        return;
    }
    std::future::pending::<()>().await
}

async fn dispatch(ev: Event, ctx: &mut Context, fx: &Effectors, telem: &mut TelemetryEmitter) {
    let kind = ev.kind();
    let label = ev.label();

    match ev {
        Event::ConfigReloaded | Event::ProfileApplied => {
            if ctx.state != State::Suspending {
                fx.ingest_active_profile(ctx);
                info!(
                    "RECONCILE ({}): re-asserting {}",
                    if matches!(ev, Event::ProfileApplied) {
                        "profile applied"
                    } else {
                        "configreloaded"
                    },
                    ctx.state.as_str()
                );
                let _ = on_enter(ctx.state, Entry::Reassert, ctx, fx).await;
            }
            fx.request_reconcile(if matches!(ev, Event::ProfileApplied) {
                ReconcileTrigger::ProfileApplied
            } else {
                ReconcileTrigger::ConfigReloaded
            });
            return;
        }
        Event::ReconcileTick(snap) => {
            handle_reconcile_tick(*snap, ctx, fx, telem).await;
            return;
        }
        Event::MonitorsChanged => {
            handle_monitors_changed(ctx, fx, telem).await;
            return;
        }
        Event::ProfilesChanged => {
            info!("PROFILE: source directory changed; re-resolving");
            fx.schedule_profile_reconcile(ctx);
            return;
        }
        Event::BacklightHotplug => {
            super::discover_backlight(ctx);
            return;
        }
        Event::Sleeping => {
            ctx.kbd_sleeping = true;
            kbd_policy_check(ctx, fx, false).await;
            return;
        }
        Event::KbdIdle { idle } => {
            ctx.kbd_typing = !idle;
            info!("KBD: {}", if idle { "input idle" } else { "typing" });
            if idle {
                fx.kbd_release_override(ctx);
            }
            kbd_policy_check(ctx, fx, !idle).await;
            return;
        }
        Event::KbdLevelChanged(level) => {
            ctx.kbd_level = level.unwrap_or(DEFAULT_KBD_LEVEL_PCT);
            info!("KBD: level {}%", ctx.kbd_level);
            kbd_policy_check(ctx, fx, true).await;
            return;
        }
        Event::KbdOverrideChanged(armed) => {
            ctx.kbd_override = armed;
            info!("KBD: override {}", if armed { "armed" } else { "cleared" });
            kbd_policy_check(ctx, fx, armed).await;
            return;
        }
        Event::KbdLedHotplug => {
            super::discover_kbd(ctx);
            kbd_policy_check(ctx, fx, true).await;
            return;
        }

        Event::Lid(closed) => {
            if ctx.lid_present {
                ctx.lid_closed = closed;
                if closed {
                    refresh_wayland_claim(ctx).await;
                }
            } else {
                warn!("lid event on a machine declared lidless (power.conf lid=absent); ignoring");
                return;
            }
        }
        Event::MonitorHotplug { added, ref name } => {
            debug!(
                "monitor {}: {name}",
                if added { "added" } else { "removed" }
            );
            ctx.ext_mon_count = hyprctl::ext_monitor_count(ctx.ext_mon_count).await;
            fx.schedule_profile_reconcile(ctx);
            fx.request_reconcile(ReconcileTrigger::MonitorHotplug);
        }
        Event::Inhibitor { wayland, active } => {
            if wayland {
                ctx.wayland_inhibitor = active;
            } else {
                ctx.logind_inhibitor = active;
            }
        }
        Event::SleepBlock(held) => {
            ctx.sleep_block = held;
        }
        Event::LockChanged(locked) => {
            ctx.locked = locked;
            fx.request_reconcile(ReconcileTrigger::LockChanged);
            kbd_policy_check(ctx, fx, false).await;
        }
        Event::AcChanged(on_ac) => {
            if on_ac == ctx.on_ac {
                debug!("AC: {label} already on_ac={on_ac}; ignoring");
                return;
            }
            ctx.on_ac = on_ac;
            info!("AC: {label} (on_ac={on_ac})");
            battery_low_request_check(ctx, fx);
            fx.schedule_power_settle(ctx);
            fx.request_reconcile(ReconcileTrigger::AcChanged);
        }
        Event::PowerAcSettled => {
            ctx.on_ac_settled = ctx.on_ac;
            battery_low_request_check(ctx, fx);
        }
        Event::PowerOverrideChanged(ref word) => match word.as_deref() {
            None => {
                ctx.power_override = None;
                ctx.power_override_base = None;
            }
            Some(w) => match w.parse::<crate::pure::power::PowerProfile>() {
                Ok(p) if ctx.power_override == Some(p) => {}
                Ok(p) => {
                    ctx.power_override = Some(p);
                    ctx.power_override_base = None;
                }
                Err(_) => warn!("power-override: unknown profile {w:?}; ignoring"),
            },
        },
        Event::BatteryPercent(pct) => {
            ctx.battery_percent = Some(pct);
            let new_low = battery_low_step(ctx.low_battery, pct, ctx.battery_low_pct);
            let flipped = new_low != ctx.low_battery;
            ctx.low_battery = new_low;
            let changed = battery_low_request_check(ctx, fx);
            if flipped {
                fx.request_reconcile(ReconcileTrigger::BatteryLowChanged);
            }
            if !flipped && !changed {
                return;
            }
        }
        Event::PlatformProfileChanged(ref value) => {
            if ctx
                .self_writes
                .is_self_write(std::time::Instant::now(), value.as_deref())
            {
                debug!("platform_profile -> {value:?}: own write");
            } else {
                let profile = profile_from_platform_value(value.as_deref());
                fx.adopt_power_override(ctx, profile);
                power_policy_check(ctx, fx).await;
            }
        }
        Event::GpuOverrideChanged(ref word) => {
            debug!("gpu-select override -> {word:?}");
        }
        Event::PowerdAppeared => {
            if !ctx.powerd_available {
                info!("powerd appeared on the bus; re-enabling power applies");
                ctx.powerd_available = true;
                ctx.powerd_warned = false;
                ctx.power_applied = None;
                ctx.dgpu_pinned = None;
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
            if requested {
                refresh_wayland_claim(ctx).await;
            }
        }
        Event::TimerExpired => {
            if ctx.suspend_requested && !crate::paths::suspend_request_standing() {
                info!("idle-suspend request withdrawn at grace expiry");
                ctx.suspend_requested = false;
            }
        }
        Event::Resumed => {
            ctx.suspend_requested = false;
            fx.clear_suspend_request();
            ctx.ext_mon_count = hyprctl::ext_monitor_count(ctx.ext_mon_count).await;
            fx.request_reconcile(ReconcileTrigger::Resumed);
            ctx.kbd_sleeping = false;
            kbd_policy_check(ctx, fx, false).await;
        }
    }

    if kind == EventKind::Resumed {
        fx.brightness_rearm(ctx);
    }

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
        let mode = gpu_drift_check(ctx, fx, label, gpu_pref);
        fx.sync_dgpu_pin(ctx, dgpu_runtime_pm_pinned(mode)).await;
    }

    if matches!(
        kind,
        EventKind::BatteryLowChanged | EventKind::PowerOverrideChanged | EventKind::PowerAcSettled
    ) {
        power_policy_check(ctx, fx).await;
        telem.emit_help(ctx, "ctx", label, ctx.state, ctx.state, Vec::new());
    }

    evaluate_fsms(ctx, fx, kind, label, telem).await;
}

#[cfg(test)]
mod battery_low_tests {
    use super::{BatteryLowAction, battery_low_action};

    #[test]
    fn requests_only_when_genuinely_discharging_low() {
        assert_eq!(
            battery_low_action(true, false, false, false),
            BatteryLowAction::Request
        );
        assert_eq!(
            battery_low_action(true, true, false, false),
            BatteryLowAction::Hold
        );
        assert_eq!(
            battery_low_action(true, false, true, false),
            BatteryLowAction::Hold
        );
        assert_eq!(
            battery_low_action(false, false, false, false),
            BatteryLowAction::Hold
        );
    }

    #[test]
    fn withdraws_the_moment_the_reason_is_gone() {
        assert_eq!(
            battery_low_action(true, true, false, true),
            BatteryLowAction::Withdraw
        );
        assert_eq!(
            battery_low_action(false, false, false, true),
            BatteryLowAction::Withdraw
        );
        assert_eq!(
            battery_low_action(true, false, false, true),
            BatteryLowAction::Hold
        );
    }
}

#[cfg(test)]
mod trace_tests {
    use super::*;
    use crate::pure::power::LidAcPolicy;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Default)]
    struct Collector(Arc<Mutex<Vec<String>>>);

    impl span_lines::Sink for Collector {
        fn write(&self, record: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(record).trim_end().to_string());
        }
    }

    fn field(line: &str, key: &str) -> String {
        line.split_whitespace()
            .filter_map(|t| t.split_once('='))
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("no {key} in {line}"))
            .1
            .to_string()
    }

    #[test]
    fn a_transition_emits_one_record_and_the_prose_log_emits_none() {
        let sink = Collector::default();
        assert!(span_lines::set_sink(sink.clone()), "test owns the sink");

        let layer = span_lines::tracing_layer::SpanLinesLayer::with_detail(
            &["hyprstate.trace"],
            span_lines::Detail::Session,
        );
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let ctx = Context {
                suspend_requested: true,
                lid_ac: LidAcPolicy::Defer,
                ..Context::default()
            };
            log_state_transition(&ctx, State::LidOpen, State::Docked, "monitor-added");
            info!("STATE: something a person reads");
        });

        let written = sink.0.lock().unwrap().clone();
        assert_eq!(
            written.len(),
            1,
            "prose must not become a record: {written:?}"
        );
        let record = &written[0];
        assert_eq!(field(record, "event"), "state.transition");
        assert_eq!(field(record, "from"), "LID_OPEN");
        assert_eq!(field(record, "to"), "DOCKED");
        assert_eq!(field(record, "cause"), "monitor-added");
        assert_eq!(field(record, "on_ac"), "true");
        assert_eq!(field(record, "suspend_requested"), "true");
        assert_eq!(field(record, "lid_ac_hold"), "true");
    }
}
