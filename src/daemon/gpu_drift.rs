//! GPU-selection drift detection: compute desired vs the session's actual
//! AQ_DRM_DEVICES; notify on mismatch (debounced). AQ_DRM_DEVICES is
//! login-time only — this never acts on the session.

use std::time::Instant;

use super::ctx::Context;
use super::effectors::Effectors;
use crate::paths;
use crate::pure::gpu::gpu_desired;
use crate::pure::profiles::GpuPref;
use crate::sysio::{gpu_state, hypr_instance, sysfs};

/// Lazily resolve the session's actual device list. Stays pending on
/// transient failure (daemon start races compositor exec) so the next
/// drift check retries.
fn resolve_gpu_actual(ctx: &mut Context) {
    let Some(devices) = hypr_instance::hyprland_aq_devices() else {
        return; // still pending
    };
    ctx.gpu_actual_pending = false;
    if !devices.is_empty() {
        ctx.gpu_actual = Some(devices);
        return;
    }
    // Var unset: normally an unmanaged session — except after a transient
    // or validation bail at select time, where the user still wants to hear
    // that a relog would now produce a managed selection.
    let reason = gpu_state::read_gpu_state().map(|s| s.reason);
    ctx.gpu_actual = match reason.as_deref() {
        Some("bailed-transient") | Some("validation-failed") => Some(Vec::new()),
        _ => None,
    };
}

/// `profile_gpu` is always the FRESH select_profile result (Auto on
/// no-match), never the stale ctx value.
pub fn gpu_drift_check(ctx: &mut Context, fx: &Effectors, trigger: &str, profile_gpu: GpuPref) {
    if ctx.gpu_actual_pending {
        resolve_gpu_actual(ctx);
    }
    if ctx.gpu_actual_pending {
        return; // not resolved yet
    }
    let Some(actual) = ctx.gpu_actual.clone() else {
        return; // unmanaged session
    };

    let override_word = sysfs::read_first_word(&paths::gpu_override_file());
    let overlay = match profile_gpu {
        GpuPref::Auto => None, // falls through, same as Python's "auto"
        pref => Some(pref.as_str().to_string()),
    };
    let platform = sysfs::read_first_word(paths::platform_profile_path());
    let (mode, source, _warnings) = crate::pure::gpu::resolve_gpu_mode(
        override_word.as_deref(),
        overlay.as_deref(),
        platform.as_deref(),
    );
    let (desired, reason) = gpu_desired(&sysfs::gpu_snapshot(), mode, source);
    let Some(desired) = desired else {
        return; // nothing actionable to advise
    };
    if desired == actual {
        ctx.gpu_last_notified = None; // re-arm: future drift notifies again
        return;
    }
    let key = desired.join(":");
    if ctx.gpu_last_notified.as_deref() == Some(key.as_str()) {
        return;
    }
    let now = Instant::now();
    if ctx
        .gpu_last_notify_at
        .is_some_and(|t| now.duration_since(t) < paths::GPU_NOTIFY_MIN)
    {
        return;
    }
    ctx.gpu_last_notified = Some(key);
    ctx.gpu_last_notify_at = Some(now);
    fx.notify_gpu_drift(&desired, &reason, trigger, ctx.on_ac);
}
