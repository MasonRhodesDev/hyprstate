//! Layer 1: narrow, idempotent world mutations.
//!
//! Two tiers: fire-and-forget subprocess effects go through a serialized
//! worker task (a slow `hyprctl reload` must never stall dispatch); effects
//! whose results feed ctx (powerd ApplyProfile, Session.Lock, Suspend,
//! SetBrightness) are awaited inline by the dispatcher, as v1 did.
//!
//! Shadow mode: every world mutation logs "[shadow] would ..." instead of
//! firing. In-memory ctx still updates so decision logs stay diffable
//! against the live v1 daemon.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::ctx::Context;
use super::event::Event;
use crate::dbus::logind::{LogindManagerProxy, LogindSessionProxy};
use crate::dbus::powerd_client::PowerdProxy;
use crate::paths;
use crate::pure::power::PowerProfile;
use crate::pure::profiles::{EdpPolicy, GpuPref, Profile};
use crate::sysio::hyprctl;

/// Serialized subprocess effects (ordering between reload and keyword
/// matters for eDP handling).
#[derive(Debug)]
pub enum Cmd {
    /// Ensure eDP enabled/disabled (read-before-write inside the worker).
    SetEdp {
        on: bool,
    },
    Reload,
    Dpms(bool),
    PauseMedia,
    Notify {
        summary: String,
        body: String,
        tag: &'static str,
    },
    RunHook(String),
}

pub async fn effector_worker(mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::SetEdp { on } => {
                // None = no eDP panel on this machine (desktop). Treat as a
                // no-op: there is nothing to (re-)enable, and reloading here
                // self-sustains — reload -> configreloaded -> RECONCILE ->
                // SetEdp -> reload (observed at ~35 reloads/s once a reload
                // storm primed it on the Lua config, 2026-07-07).
                let Some(disabled) = hyprctl::edp_is_disabled().await else {
                    continue;
                };
                if on {
                    if !disabled {
                        continue;
                    }
                    info!("re-enabling {} via hyprctl reload", hyprctl::EDP_MONITOR);
                    run_cmd("hyprctl", &["reload"]).await;
                } else {
                    if disabled {
                        continue;
                    }
                    info!("disabling {}", hyprctl::EDP_MONITOR);
                    let arg = format!("{},disable", hyprctl::EDP_MONITOR);
                    run_cmd("hyprctl", &["keyword", "monitor", &arg]).await;
                }
            }
            Cmd::Reload => run_cmd("hyprctl", &["reload"]).await,
            Cmd::Dpms(on) => {
                run_cmd(
                    "hyprctl",
                    &["dispatch", "dpms", if on { "on" } else { "off" }],
                )
                .await
            }
            Cmd::PauseMedia => run_cmd("playerctl", &["--all-players", "pause"]).await,
            Cmd::Notify { summary, body, tag } => {
                let hint = format!("string:x-canonical-private-synchronous:{tag}");
                run_cmd(
                    "notify-send",
                    &["-a", "hyprstate", "-h", &hint, &summary, &body],
                )
                .await;
            }
            Cmd::RunHook(cmd) => {
                // Hooks are user-authored; shell for ~ expansion etc.
                match tokio::process::Command::new("bash")
                    .args(["-lc", &cmd])
                    .spawn()
                {
                    Ok(_) => {}
                    Err(e) => warn!("hook {cmd:?} failed to launch: {e}"),
                }
            }
        }
    }
}

async fn run_cmd(cmd: &str, args: &[&str]) {
    match tokio::process::Command::new(cmd).args(args).output().await {
        Ok(out) if !out.status.success() => {
            warn!(
                "command failed: {cmd} {args:?} (rc={:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(_) => {}
        Err(e) => warn!("command failed to spawn: {cmd} {args:?}: {e}"),
    }
}

pub struct Effectors {
    pub shadow: bool,
    pub worker: mpsc::Sender<Cmd>,
    pub queue: mpsc::Sender<Event>,
    pub manager: LogindManagerProxy<'static>,
    pub session: Option<LogindSessionProxy<'static>>,
    pub powerd: PowerdProxy<'static>,
    pub locked_rx: watch::Receiver<bool>,
}

impl Effectors {
    fn send_cmd(&self, cmd: Cmd) {
        if self.shadow {
            info!("[shadow] would run effect: {cmd:?}");
            return;
        }
        if self.worker.try_send(cmd).is_err() {
            warn!("effector worker queue full/closed — effect dropped");
        }
    }

    // ---- eDP / dpms / media / notify ----

    /// The active profile's edp policy overrides the lid-driven default.
    pub fn set_edp(&self, on: bool, ctx: &Context) {
        let resolved = match ctx.edp_policy {
            EdpPolicy::Disable => false,
            EdpPolicy::Enable => true,
            EdpPolicy::Auto => on,
        };
        self.send_cmd(Cmd::SetEdp { on: resolved });
    }

    pub fn dpms(&self, on: bool) {
        self.send_cmd(Cmd::Dpms(on));
    }

    pub fn pause_media(&self) {
        self.send_cmd(Cmd::PauseMedia);
    }

    pub fn notify_power(&self, body: String) {
        info!("POWER: notify: {body}");
        self.send_cmd(Cmd::Notify {
            summary: "Power profile".into(),
            body,
            tag: "hyprstate-power",
        });
    }

    pub fn notify_gpu_drift(&self, desired: &[String], reason: &str, trigger: &str, on_ac: bool) {
        let primary = desired
            .first()
            .map(|d| d.rsplit('/').next().unwrap_or(d).to_string())
            .unwrap_or_default();
        let mut body = format!("{trigger}: a relog would switch rendering to {primary} ({reason})");
        if !on_ac {
            body += " — on battery";
        }
        info!(
            "GPU drift: desired={} reason={reason} trigger={trigger}",
            desired.join(":")
        );
        self.send_cmd(Cmd::Notify {
            summary: "GPU selection drift".into(),
            body,
            tag: "hyprstate-gpu",
        });
    }

    // ---- timers ----

    fn spawn_timer(
        &self,
        delay: Duration,
        make_event: fn() -> Event,
    ) -> tokio::task::JoinHandle<()> {
        let tx = self.queue.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(make_event()).await;
        })
    }

    pub fn cancel_grace_timer(&self, ctx: &mut Context) {
        if let Some(t) = ctx.grace_timer.take() {
            t.abort();
        }
    }

    /// `fresh` = entering COUNTDOWN; a RECONCILE re-assert must NOT restart
    /// (and thereby extend) a live countdown.
    pub fn start_grace_timer(&self, ctx: &mut Context, fresh: bool) {
        if !fresh && ctx.grace_timer.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        self.cancel_grace_timer(ctx);
        ctx.grace_timer = Some(self.spawn_timer(paths::GRACE_SECONDS, || Event::TimerExpired));
    }

    pub fn cancel_screen_timer(&self, ctx: &mut Context) {
        if let Some(t) = ctx.screen_timer.take() {
            t.abort();
        }
    }

    pub fn start_screen_timer(&self, ctx: &mut Context, fresh: bool) {
        if !fresh && ctx.screen_timer.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        self.cancel_screen_timer(ctx);
        ctx.screen_timer = Some(self.spawn_timer(paths::DPMS_DELAY, || Event::ScreenTimerExpired));
    }

    /// Debounce monitor add/remove bursts into one MonitorsChanged.
    pub fn schedule_profile_reconcile(&self, ctx: &mut Context) {
        if let Some(t) = ctx.profile_debounce.take() {
            t.abort();
        }
        ctx.profile_debounce =
            Some(self.spawn_timer(paths::PROFILE_DEBOUNCE, || Event::MonitorsChanged));
    }

    /// Debounce raw AC flips; PowerAcSettled fires once the state stops
    /// bouncing.
    pub fn schedule_power_settle(&self, ctx: &mut Context) {
        if let Some(t) = ctx.power_debounce.take() {
            t.abort();
        }
        ctx.power_debounce =
            Some(self.spawn_timer(paths::POWER_AC_DEBOUNCE, || Event::PowerAcSettled));
    }

    // ---- monitor profiles ----

    /// Repoint the active-profile symlink, reload, fire hooks, update ctx.
    /// Idempotent.
    pub fn apply_profile(&self, profile: &Profile, ctx: &mut Context) {
        let target =
            paths::profiles_dir().join(format!("{}.{}", profile.name, profile.format.ext()));
        let link = paths::active_profile_link(profile.format);
        let already = link.is_symlink()
            && fs::canonicalize(&link).ok() == fs::canonicalize(&target).ok()
            && ctx.current_profile.as_deref() == Some(profile.name.as_str());
        if already {
            return;
        }

        if self.shadow {
            info!(
                "[shadow] PROFILE: {} -> {} (edp={}, hooks={}) — would repoint+reload",
                ctx.current_profile.as_deref().unwrap_or("None"),
                profile.name,
                profile.edp.as_str(),
                profile.hooks.len()
            );
        } else {
            if let Err(e) = crate::sysio::profiles::repoint_active_profile(&target) {
                warn!("apply_profile {}: symlink failed: {e}", profile.name);
                return;
            }
            info!(
                "PROFILE: {} -> {} (edp={}, hooks={})",
                ctx.current_profile.as_deref().unwrap_or("None"),
                profile.name,
                profile.edp.as_str(),
                profile.hooks.len()
            );
        }
        ctx.current_profile = Some(profile.name.clone());
        ctx.edp_policy = profile.edp;

        if !self.shadow {
            self.send_cmd(Cmd::Reload);
            for hook in &profile.hooks {
                self.send_cmd(Cmd::RunHook(hook.clone()));
            }
        }
    }

    /// Sync ctx with an out-of-band .active.* repoint (`profile switch`).
    /// Returns true when ingested. The daemon's own apply_profile updates
    /// ctx synchronously, so this only fires for external changes.
    pub fn ingest_active_profile(&self, ctx: &mut Context) -> bool {
        let Some(name) = crate::sysio::profiles::active_profile_name() else {
            return false;
        };
        if ctx.current_profile.as_deref() == Some(name.as_str()) {
            return false;
        }
        let edp = crate::sysio::profiles::load_profiles()
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.edp)
            .unwrap_or(EdpPolicy::Auto);
        info!(
            "PROFILE: ingested external switch {} -> {name} (edp={})",
            ctx.current_profile.as_deref().unwrap_or("None"),
            edp.as_str()
        );
        ctx.current_profile = Some(name);
        ctx.edp_policy = edp;
        true
    }

    /// Record the matched profile's `#@ gpu` for next login's `gpu select`.
    /// Cleared on no-match/auto so the file only exists when it means
    /// something.
    pub fn write_gpu_breadcrumb(&self, value: Option<GpuPref>) {
        if self.shadow {
            debug!("[shadow] would write gpu breadcrumb: {value:?}");
            return;
        }
        let file = paths::gpu_breadcrumb_file();
        let result = match value {
            None | Some(GpuPref::Auto) => {
                if file.exists() {
                    fs::remove_file(&file)
                } else {
                    Ok(())
                }
            }
            Some(pref) => {
                if let Some(parent) = file.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::write(&file, format!("{}\n", pref.as_str()))
            }
        };
        if let Err(e) = result {
            warn!("gpu breadcrumb write failed: {e}");
        }
    }

    // ---- power ----

    /// Ask powerd to apply a profile. No-op when already applied. Registers
    /// the expected platform_profile chain BEFORE the call so the poller
    /// can't observe sysfs ahead of the bookkeeping.
    pub async fn apply_power_profile(&self, ctx: &mut Context, profile: PowerProfile) {
        if ctx.power_applied == Some(profile) || !ctx.powerd_available {
            return;
        }
        ctx.self_writes.register(Instant::now(), profile);
        if self.shadow {
            info!("[shadow] POWER: would apply {}", profile.as_str());
            ctx.power_applied = Some(profile);
            return;
        }
        match self.powerd.apply_profile(profile.as_str()).await {
            Ok(results) => {
                ctx.power_applied = Some(profile);
                let interesting: std::collections::BTreeMap<_, _> = results
                    .iter()
                    .filter(|(_, v)| *v != "unchanged" && *v != "skipped-missing")
                    .collect();
                if interesting.is_empty() {
                    info!("POWER: applied {} (all unchanged)", profile.as_str());
                } else {
                    info!("POWER: applied {} ({interesting:?})", profile.as_str());
                }
            }
            Err(e) => {
                ctx.powerd_available = false; // NameOwnerChanged re-enables
                if !ctx.powerd_warned {
                    ctx.powerd_warned = true;
                    warn!(
                        "powerd unavailable ({e}) — power profiles disabled until {} appears",
                        paths::POWERD_BUS
                    );
                }
            }
        }
    }

    /// Pin (`awake=true`) or release the discrete GPU's runtime PM via powerd.
    /// `awake` should be `pure::gpu::dgpu_runtime_pm_pinned(mode)`: dgpu mode
    /// must block D3cold (the FW16 DCN resume wedge). Idempotent on
    /// `ctx.dgpu_pinned`; deferred while powerd is unavailable (re-pushed when
    /// PowerdAppeared resets the marker).
    pub async fn sync_dgpu_pin(&self, ctx: &mut Context, awake: bool) {
        if ctx.dgpu_pinned == Some(awake) || !ctx.powerd_available {
            return;
        }
        if self.shadow {
            info!("[shadow] GPU: would set dgpu pin awake={awake}");
            ctx.dgpu_pinned = Some(awake);
            return;
        }
        match self.powerd.set_dgpu_awake(awake).await {
            Ok(results) => {
                ctx.dgpu_pinned = Some(awake);
                let interesting: std::collections::BTreeMap<_, _> = results
                    .iter()
                    .filter(|(_, v)| *v != "unchanged" && *v != "skipped-missing")
                    .collect();
                if interesting.is_empty() {
                    info!("GPU: dgpu pin awake={awake} (all unchanged)");
                } else {
                    info!("GPU: dgpu pin awake={awake} ({interesting:?})");
                }
            }
            Err(e) => {
                ctx.powerd_available = false; // NameOwnerChanged re-enables
                if !ctx.powerd_warned {
                    ctx.powerd_warned = true;
                    warn!("powerd unavailable ({e}) — dgpu pin deferred");
                }
            }
        }
    }

    /// Delete the override file and update ctx SYNCHRONOUSLY — the poller
    /// echo of the deletion must arrive as a no-op.
    pub fn clear_power_override(&self, ctx: &mut Context) {
        ctx.power_override = None;
        ctx.power_override_base = None;
        if self.shadow {
            info!("[shadow] would delete power override file");
            return;
        }
        if let Err(e) = fs::remove_file(paths::power_override_file())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("power override clear failed: {e}");
        }
    }

    /// External platform_profile write -> adopt as override (never
    /// revert-fight). File + ctx updated synchronously.
    pub fn adopt_power_override(&self, ctx: &mut Context, profile: PowerProfile) {
        ctx.power_override = Some(profile);
        ctx.power_override_base = None; // re-stamped on next policy check
        if self.shadow {
            info!("[shadow] would adopt power override: {}", profile.as_str());
        } else {
            let file = paths::power_override_file();
            if let Some(parent) = file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::write(&file, format!("{}\n", profile.as_str())) {
                warn!("power override write failed: {e}");
            }
        }
        self.notify_power(format!(
            "Adopted external power change as override: {}",
            profile.as_str()
        ));
    }

    // ---- brightness (logind SetBrightness — session-scoped) ----

    fn brightness_read(&self, ctx: &Context) -> Option<u32> {
        let dev = ctx.brightness_dev.as_ref()?;
        let text = fs::read_to_string(
            Path::new("/sys/class/backlight")
                .join(dev)
                .join("brightness"),
        )
        .ok()?;
        text.trim().parse().ok()
    }

    /// Guard at ±0.5% of max: any larger delta from what we last set is a
    /// deliberate user adjustment — leave it alone.
    fn brightness_user_took_over(&self, ctx: &Context) -> bool {
        let Some(set) = ctx.brightness_set else {
            return false;
        };
        let Some(cur) = self.brightness_read(ctx) else {
            return false;
        };
        let slack = (ctx.brightness_max as f64 * 0.005) as i64;
        (cur as i64 - set as i64).abs() > slack
    }

    async fn brightness_write(&self, ctx: &mut Context, value: u32) {
        let Some(dev) = ctx.brightness_dev.clone() else {
            debug!("brightness: no device — skipping");
            return;
        };
        let Some(session) = &self.session else {
            debug!("brightness: no session — skipping");
            return;
        };
        let value = value.clamp(1, ctx.brightness_max);
        if self.shadow {
            info!("[shadow] would set brightness {dev} = {value}");
            ctx.brightness_set = Some(value);
            return;
        }
        match session.set_brightness("backlight", &dev, value).await {
            Ok(()) => ctx.brightness_set = Some(value),
            Err(e) => warn!("SetBrightness failed: {e}"),
        }
    }

    pub async fn brightness_save_and_cap(&self, ctx: &mut Context, pct: f64) {
        if self.brightness_user_took_over(ctx) {
            ctx.brightness_saved = None;
            ctx.brightness_set = self.brightness_read(ctx);
            return;
        }
        let Some(cur) = self.brightness_read(ctx) else {
            return;
        };
        ctx.brightness_saved = Some(cur);
        let cap = (ctx.brightness_max as f64 * pct) as u32;
        if cur > cap {
            self.brightness_write(ctx, cap).await;
        }
    }

    pub async fn brightness_cap(&self, ctx: &mut Context, pct: f64) {
        if self.brightness_user_took_over(ctx) {
            ctx.brightness_saved = None;
            ctx.brightness_set = self.brightness_read(ctx);
            return;
        }
        let Some(cur) = self.brightness_read(ctx) else {
            return;
        };
        let cap = (ctx.brightness_max as f64 * pct) as u32;
        if cur > cap {
            self.brightness_write(ctx, cap).await;
        }
    }

    pub async fn brightness_restore(&self, ctx: &mut Context) {
        let Some(saved) = ctx.brightness_saved.take() else {
            return;
        };
        if self.brightness_user_took_over(ctx) {
            ctx.brightness_set = self.brightness_read(ctx);
            return;
        }
        self.brightness_write(ctx, saved).await;
    }

    /// Re-arm the takeover guard (RESUMED: panel raw values drift across
    /// suspend and must not read as a user adjustment).
    pub fn brightness_rearm(&self, ctx: &mut Context) {
        if ctx.brightness_dev.is_some() {
            ctx.brightness_set = self.brightness_read(ctx);
        }
    }

    // ---- lock / suspend ----

    pub async fn request_lock(&self) {
        let Some(session) = &self.session else {
            warn!("no session proxy — cannot request lock");
            return;
        };
        info!("requesting Session.Lock()");
        if self.shadow {
            info!("[shadow] would call Session.Lock()");
            return;
        }
        if let Err(e) = session.lock().await {
            warn!("Session.Lock() failed: {e}");
        }
    }

    /// Wait for the lock to engage (watch channel fed by the LockedHint
    /// subscription), up to LOCK_WAIT.
    pub async fn wait_for_lock(&self, ctx: &Context) -> bool {
        if ctx.locked {
            return true;
        }
        let mut rx = self.locked_rx.clone();
        tokio::time::timeout(paths::LOCK_WAIT, rx.wait_for(|locked| *locked))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    pub async fn do_suspend(&self) {
        info!("calling logind Suspend()");
        if self.shadow {
            info!("[shadow] would call Suspend(false)");
            return;
        }
        if let Err(e) = self.manager.suspend(false).await {
            warn!("Suspend() failed: {e}");
        }
    }
}
