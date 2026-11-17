use std::time::Instant;

use super::ctx::Context;
use super::effectors::Effectors;
use crate::paths;
use crate::pure::gpu::{GpuMode, GpuModeSource, gpu_desired, resolve_gpu_mode};
use crate::pure::profiles::GpuPref;
use crate::sysio::{gpu_state, hypr_instance, sysfs};

fn resolve_gpu_actual(ctx: &mut Context) {
    let Some(devices) = hypr_instance::hyprland_aq_devices() else {
        return;
    };
    ctx.gpu_actual_pending = false;
    if !devices.is_empty() {
        ctx.gpu_actual = Some(devices);
        return;
    }
    let reason = gpu_state::read_gpu_state().map(|s| s.reason);
    ctx.gpu_actual = match reason.as_deref() {
        Some("bailed-transient") | Some("validation-failed") => Some(Vec::new()),
        _ => None,
    };
}

pub fn resolve_session_gpu_mode(profile_gpu: GpuPref) -> (GpuMode, GpuModeSource) {
    let override_word = sysfs::read_first_word(&paths::gpu_override_file());
    let overlay = match profile_gpu {
        GpuPref::Auto => None,
        pref => Some(pref.as_str().to_string()),
    };
    let platform = sysfs::read_first_word(paths::platform_profile_path());
    let (mode, source, _warnings) = resolve_gpu_mode(
        override_word.as_deref(),
        overlay.as_deref(),
        platform.as_deref(),
    );
    (mode, source)
}

pub fn gpu_drift_check(
    ctx: &mut Context,
    fx: &Effectors,
    trigger: &str,
    profile_gpu: GpuPref,
) -> GpuMode {
    let (mode, source) = resolve_session_gpu_mode(profile_gpu);
    check_and_notify_drift(ctx, fx, trigger, mode, source);
    mode
}

fn check_and_notify_drift(
    ctx: &mut Context,
    fx: &Effectors,
    trigger: &str,
    mode: GpuMode,
    source: GpuModeSource,
) {
    if ctx.gpu_actual_pending {
        resolve_gpu_actual(ctx);
    }
    if ctx.gpu_actual_pending {
        return;
    }
    let Some(actual) = ctx.gpu_actual.clone() else {
        return;
    };
    let (desired, reason) = gpu_desired(&sysfs::gpu_snapshot(), mode, source);
    let Some(desired) = desired else {
        return;
    };
    if desired == actual {
        ctx.gpu_last_notified = None;
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
