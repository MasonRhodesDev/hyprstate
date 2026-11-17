use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::ctx::Context;
use super::effectors::Effectors;
use crate::paths;
use crate::pure::kbd::{KbdInputs, is_dark, kbd_lit};
use crate::sysio::kbd::read_lux;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbdStatus {
    pub lit: bool,
    pub level: u8,
    pub override_armed: bool,
    pub typing: bool,
    pub dark: bool,
    pub lux: Option<f64>,
    pub dark_lux: u32,
    pub led: String,
    pub sensor: String,
}

impl KbdStatus {
    pub fn of(ctx: &Context, lit: bool) -> Self {
        KbdStatus {
            lit,
            level: ctx.kbd_level,
            override_armed: ctx.kbd_override,
            typing: ctx.kbd_typing,
            dark: ctx.kbd_dark,
            lux: ctx.kbd_last_lux,
            dark_lux: ctx.kbd_dark_lux,
            led: ctx
                .kbd_led
                .as_ref()
                .map(|(n, _)| n.clone())
                .unwrap_or_default(),
            sensor: ctx
                .kbd_als
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        }
    }
}

pub async fn kbd_policy_check(ctx: &mut Context, fx: &Effectors, sample: bool) {
    if !ctx.kbd_active() {
        return;
    }
    if sample && let Some(als) = &ctx.kbd_als {
        match read_lux(als) {
            Some(lux) => {
                ctx.kbd_last_lux = Some(lux);
                ctx.kbd_dark = is_dark(lux, ctx.kbd_dark_lux);
            }
            None => warn!(
                "kbd: sensor {} unreadable; keeping dark={}",
                als.display(),
                ctx.kbd_dark
            ),
        }
    }
    let inputs = KbdInputs {
        locked: ctx.locked,
        sleeping: ctx.kbd_sleeping,
        typing: ctx.kbd_typing,
        dark: ctx.kbd_dark,
        override_armed: ctx.kbd_override,
    };
    let lit = kbd_lit(&inputs);
    debug!("kbd: {inputs:?} -> lit={lit}");
    fx.kbd_set(ctx, if lit { ctx.kbd_level } else { 0 }).await;
    fx.kbd_write_status(ctx, lit);
}

pub fn status_path() -> Option<std::path::PathBuf> {
    paths::kbd_status_file()
}
