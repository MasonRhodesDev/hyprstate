//! The event loop: ctx updates, the exact v1 routing topology, and Layer-2
//! on_enter composition.
//!
//! Routing invariants ported from v1 (hyprstate.py dispatcher):
//! - RECONCILE (configreloaded) re-asserts the current states only — it
//!   never feeds desired_state — and ingests .active.conf first.
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
use super::telemetry::{FrameCtx, TelemetryEmitter, build_frame};
use crate::pure::fsm::{
    EventKind, ScreenState, State, desired_screen_state, desired_state, world_state,
};
use crate::pure::gpu::dgpu_runtime_pm_pinned;
use crate::pure::power::{battery_low_step, profile_from_platform_value};
use crate::pure::profiles::{EdpPolicy, GpuPref, select_profile};
use crate::sysio::hyprctl;
use crate::sysio::profiles::load_profiles;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Fresh,
    Reassert,
}

async fn on_enter(state: State, entry: Entry, ctx: &mut Context, fx: &Effectors) {
    match state {
        State::LidOpen => {
            fx.cancel_grace_timer(ctx);
            fx.set_edp(true, ctx);
        }
        State::Docked => {
            fx.cancel_grace_timer(ctx);
            fx.set_edp(false, ctx);
        }
        State::Deferred => {
            fx.cancel_grace_timer(ctx);
            fx.set_edp(false, ctx);
            fx.pause_media();
        }
        State::Countdown => {
            fx.set_edp(false, ctx);
            // A RECONCILE re-assert must NOT restart (extend) a live
            // countdown — v1 silently reset the 30s window on every
            // configreloaded.
            fx.start_grace_timer(ctx, entry == Entry::Fresh);
        }
        State::Suspending => {
            fx.cancel_grace_timer(ctx);
            suspending_tail(ctx, fx).await;
        }
    }
}

/// Lock-before-suspend: trigger lock, wait up to LOCK_WAIT, then suspend
/// regardless.
async fn suspending_tail(ctx: &mut Context, fx: &Effectors) {
    if !ctx.locked {
        fx.request_lock().await;
        if fx.wait_for_lock(ctx).await {
            info!("lock engaged; proceeding to suspend");
        } else {
            warn!("lock did not engage in 2.0s — suspending anyway");
        }
    } else {
        info!("already locked; proceeding to suspend");
    }
    fx.do_suspend().await;
}

async fn on_enter_screen(
    prev: ScreenState,
    state: ScreenState,
    entry: Entry,
    ctx: &mut Context,
    fx: &Effectors,
) {
    match state {
        ScreenState::Active => {
            fx.cancel_screen_timer(ctx);
            // Only wake screens when WE dimmed them (transition out of
            // DIMMED) — v1 fired dpms(on) on every config reload, fighting
            // hypridle's own idle DPMS.
            if prev == ScreenState::Dimmed {
                fx.dpms(true);
            }
        }
        ScreenState::DimPending => {
            fx.start_screen_timer(ctx, entry == Entry::Fresh);
        }
        ScreenState::Dimmed => {
            fx.cancel_screen_timer(ctx);
            fx.dpms(false);
        }
    }
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
        ctx.state = new;
        on_enter(new, Entry::Fresh, ctx, fx).await;

        // Best-effort telemetry — never affects FSM behavior.
        let frame = build_frame(
            from,
            new,
            kind,
            label,
            ctx.screen_state,
            FrameCtx {
                lid_closed: ctx.lid_closed,
                ext_mon_count: ctx.ext_mon_count,
                inhibitor: ctx.inhibitor(),
                locked: ctx.locked,
                on_ac: ctx.on_ac,
            },
            on_enter_effector_names(new),
        );
        telem.emit(&frame);
    } else {
        debug!(
            "ignored: {label} in {} (ext_mon={}, inhibitor={}, locked={}, on_ac={})",
            ctx.state.as_str(),
            ctx.ext_mon_count,
            ctx.inhibitor(),
            ctx.locked,
            ctx.on_ac,
        );
    }

    if let Some(new) = desired_screen_state(ctx.state, ctx.screen_state, kind, &ctx.screen_inputs())
        && new != ctx.screen_state
    {
        info!(
            "SCREEN: {} -> {} (event={label}, locked={}, inhibitor={}, main={})",
            ctx.screen_state.as_str(),
            new.as_str(),
            ctx.locked,
            ctx.inhibitor(),
            ctx.state.as_str(),
        );
        let prev = ctx.screen_state;
        ctx.screen_state = new;
        on_enter_screen(prev, new, Entry::Fresh, ctx, fx).await;
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
async fn handle_monitors_changed(ctx: &mut Context, fx: &Effectors) {
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
        }
        Some(p) if ctx.current_profile.as_deref() != Some(p.name.as_str()) => {
            fx.apply_profile(p, ctx);
        }
        Some(p) => debug!("PROFILE: signature change but {} still wins", p.name),
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

    if snap.lid_closed != ctx.lid_closed {
        drift.push(format!(
            "lid_closed {}->{}",
            ctx.lid_closed, snap.lid_closed
        ));
        ctx.lid_closed = snap.lid_closed;
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
    if snap.locked != ctx.locked {
        drift.push(format!(
            "locked {}->{} (pgrep fallback)",
            ctx.locked, snap.locked
        ));
        ctx.locked = snap.locked;
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
    }
    // Repaired FSM inputs must DRIVE the machines, not just describe them.
    if fsm_drift {
        evaluate_fsms(ctx, fx, EventKind::CtxRepaired, "CtxRepaired", telem).await;
    }

    if ctx.state == State::Suspending {
        return;
    }

    // Ingest any out-of-band .active.conf repoint before enforcing the eDP
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

    // DPMS-DIMMED invariant: re-issue dpms off (idempotent).
    if ctx.screen_state == ScreenState::Dimmed {
        fx.dpms(false);
    }
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
    on_enter(ctx.state, Entry::Fresh, &mut ctx, &fx).await;

    // Pre-evaluate the sub-FSM in case we start in LID_OPEN/DOCKED already
    // locked + inhibited.
    if let Some(new) = desired_screen_state(
        ctx.state,
        ctx.screen_state,
        EventKind::Reconcile,
        &ctx.screen_inputs(),
    ) && new != ctx.screen_state
    {
        info!(
            "SCREEN: {} -> {} (initial)",
            ctx.screen_state.as_str(),
            new.as_str()
        );
        let prev = ctx.screen_state;
        ctx.screen_state = new;
        on_enter_screen(prev, new, Entry::Fresh, &mut ctx, &fx).await;
    }

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
                        "RECONCILE (configreloaded): re-asserting {}/{}",
                        ctx.state.as_str(),
                        ctx.screen_state.as_str()
                    );
                    on_enter(ctx.state, Entry::Reassert, &mut ctx, &fx).await;
                    on_enter_screen(
                        ctx.screen_state,
                        ctx.screen_state,
                        Entry::Reassert,
                        &mut ctx,
                        &fx,
                    )
                    .await;
                }
                continue;
            }
            Event::ReconcileTick(snap) => {
                handle_reconcile_tick(*snap, &mut ctx, &fx, &mut telem).await;
                continue;
            }
            Event::MonitorsChanged => {
                handle_monitors_changed(&mut ctx, &fx).await;
                continue; // profile reconciliation does not feed the main FSM
            }

            // ---- ctx updates that fall through to the FSMs ----
            Event::Lid(closed) => ctx.lid_closed = closed,
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
                fx.schedule_power_settle(&mut ctx);
            }
            Event::PowerAcSettled => ctx.on_ac_settled = ctx.on_ac,
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
                if new_low == ctx.low_battery {
                    continue; // no flip -> no event existed in v1
                }
                ctx.low_battery = new_low;
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
            Event::TimerExpired | Event::ScreenTimerExpired | Event::Resumed => {}
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
        }

        evaluate_fsms(&mut ctx, &fx, kind, label, &mut telem).await;
    }
}
