//! Power policy orchestration: base state -> override stamping/expiry ->
//! desired profile + brightness edges. Idempotent on unchanged inputs (the
//! mode poller echoes the daemon's own override-file writes ~2s later —
//! that echo must land as a no-op).

use tracing::info;

use super::ctx::Context;
use super::effectors::Effectors;
use crate::pure::power::{AcAxis, BaseState, ac_axis, power_base_state};

pub async fn power_policy_check(ctx: &mut Context, fx: &Effectors) {
    let base = power_base_state(ctx.on_ac_settled, ctx.ext_mon_count, ctx.low_battery);

    if let Some(override_profile) = ctx.power_override {
        match ctx.power_override_base {
            None => {
                // First ingest: the daemon stamps the base itself — the CLI
                // can't know the hysteresis-adjusted state.
                ctx.power_override_base = Some(base);
                info!(
                    "POWER: override {} stamped at base={}",
                    override_profile.as_str(),
                    base.as_str()
                );
            }
            Some(stamped)
                if ac_axis(base) != ac_axis(stamped)
                    || (base == BaseState::BatteryLow && stamped != BaseState::BatteryLow) =>
            {
                // Expiry: AC axis flip or battery-low entry only.
                // docked-ac <-> ac never expires (a display blink must not
                // delete explicit intent).
                info!(
                    "POWER: override {} expired (base {} -> {})",
                    override_profile.as_str(),
                    stamped.as_str(),
                    base.as_str()
                );
                fx.clear_power_override(ctx);
                fx.notify_power(format!(
                    "Power override cleared — back to automatic ({} on {})",
                    ctx.power_policy.for_base(base).as_str(),
                    base.as_str()
                ));
            }
            Some(_) => {}
        }
    }

    let desired = ctx
        .power_override
        .unwrap_or_else(|| ctx.power_policy.for_base(base));
    fx.apply_power_profile(ctx, desired).await;

    // Brightness on base-state EDGES only (never on manual profile changes).
    let prev = ctx.power_last_base;
    ctx.power_last_base = Some(base);
    let Some(prev) = prev else {
        return;
    };
    if prev == base {
        return;
    }
    if ac_axis(prev) == AcAxis::Ac && ac_axis(base) == AcAxis::Battery {
        fx.brightness_save_and_cap(ctx, 0.50).await;
    }
    if base == BaseState::BatteryLow && prev != BaseState::BatteryLow {
        fx.brightness_cap(ctx, 0.25).await;
    }
    if ac_axis(prev) == AcAxis::Battery && ac_axis(base) == AcAxis::Ac {
        fx.brightness_restore(ctx).await;
    }
}
