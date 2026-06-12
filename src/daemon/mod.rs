//! `hyprstate daemon`: the lid/monitor/profile/lock/suspend/power state
//! machine (systemd --user). `--shadow` runs everything except effects: no
//! lid inhibitor, all world mutations logged instead of fired — safe to run
//! alongside the live v1 daemon for decision-log diffing.

pub mod ctx;
pub mod dispatcher;
pub mod effectors;
pub mod event;
pub mod gpu_drift;
pub mod power_policy;
pub mod sources;

use std::fs;
use std::path::Path;

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use zbus::proxy::CacheProperties;

use crate::dbus::logind::LogindManagerProxy;
use crate::dbus::powerd_client::PowerdProxy;
use crate::dbus::upower::UPowerProxy;
use crate::paths;
use crate::sysio::{hyprctl, hypridle_log, sysfs};
use ctx::Context;
use effectors::Effectors;
use event::Event;

/// Startup diagnostic only — the sleep hook owns the fix.
fn log_wake_state() {
    let mut entries: Vec<String> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    let mut record = |label: String, path: &Path| {
        if let Ok(text) = fs::read_to_string(path) {
            let state = text.trim().to_string();
            if state != "enabled" {
                bad.push(label.clone());
            }
            entries.push(format!("{label}={state}"));
        }
    };
    record("controller".into(), Path::new(paths::WAKE_USB_CONTROLLER));
    if let Ok(rd) = fs::read_dir("/sys/bus/usb/devices") {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with("usb") {
                record(name, &path.join("power/wakeup"));
                continue;
            }
            let (Ok(v), Ok(p)) = (
                fs::read_to_string(path.join("idVendor")),
                fs::read_to_string(path.join("idProduct")),
            ) else {
                continue;
            };
            for ((vid, pid), label) in paths::WAKE_USB_VENDORS {
                if (v.trim(), p.trim()) == (vid, pid) {
                    record(label.to_string(), &path.join("power/wakeup"));
                }
            }
        }
    }
    if entries.is_empty() {
        info!("usb-wake state: (none of the tracked devices found)");
    } else {
        info!("usb-wake state: {}", entries.join(" "));
        if !bad.is_empty() {
            warn!(
                "wake disabled on: {} — check sleep hook install",
                bad.join(", ")
            );
        }
    }
}

fn discover_backlight(ctx: &mut Context) {
    let Ok(rd) = fs::read_dir("/sys/class/backlight") else {
        return;
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("ddcci"))
        .collect();
    names.sort();
    let preferred = names.iter().find(|n| {
        n.starts_with("amdgpu_bl")
            || n.starts_with("intel_backlight")
            || n.starts_with("acpi_video")
    });
    let Some(dev) = preferred.or(names.first()).cloned() else {
        return;
    };
    let max = sysfs::read_int(
        &Path::new("/sys/class/backlight")
            .join(&dev)
            .join("max_brightness"),
        0,
    ) as u32;
    if max == 0 {
        return;
    }
    info!("backlight: {dev} (max={max})");
    ctx.brightness_dev = Some(dev);
    ctx.brightness_max = max;
}

pub async fn run(shadow: bool) -> anyhow::Result<()> {
    if shadow {
        info!("SHADOW MODE: effects logged, not fired; no lid inhibitor taken");
    }
    log_wake_state();

    let conn = zbus::Connection::system().await?;
    let manager = LogindManagerProxy::new(&conn).await?;
    // The reconciler exists to catch missed PropertiesChanged signals — it
    // must read fresh values, not the signal-fed cache.
    let manager_uncached = LogindManagerProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let (tx, rx) = mpsc::channel::<Event>(256);
    let (worker_tx, worker_rx) = mpsc::channel(64);
    let (locked_tx, locked_rx) = watch::channel(false);

    let mut ctx = Context::default();
    let (policy, low_pct) = crate::sysio::power_conf::load_power_policy();
    ctx.power_policy = policy;
    ctx.battery_low_pct = low_pct;

    // Lid inhibitor first (held for process lifetime; dropping releases).
    let _lid_inhibit_fd = if shadow {
        None
    } else {
        match manager
            .inhibit(
                "handle-lid-switch",
                "hyprstate",
                "30s grace window with monitor/inhibitor cancellation",
                "block",
            )
            .await
        {
            Ok(fd) => {
                info!("acquired handle-lid-switch inhibitor");
                Some(fd)
            }
            Err(e) => {
                warn!("could not take handle-lid-switch inhibitor: {e}");
                None
            }
        }
    };

    // Session + UPower setup.
    let session = sources::resolve_session(&conn, &manager).await;
    let upower = match UPowerProxy::new(&conn).await {
        Ok(up) => match up.on_battery().await {
            Ok(on_battery) => {
                ctx.on_ac = !on_battery;
                ctx.on_ac_settled = ctx.on_ac;
                Some(up)
            }
            Err(e) => {
                warn!("initial OnBattery read failed: {e}");
                Some(up)
            }
        },
        Err(e) => {
            warn!("UPower interface unavailable: {e} — on_ac left at default");
            None
        }
    };
    let battery = if upower.is_some() {
        sources::battery_initial(&conn).await
    } else {
        None
    };
    match &battery {
        Some((pct, _)) => {
            ctx.battery_percent = Some(*pct);
            ctx.low_battery = *pct <= ctx.battery_low_pct as f64;
            info!(
                "battery: {pct:.0}% (low={}, threshold={})",
                ctx.low_battery, ctx.battery_low_pct
            );
        }
        None => {
            ctx.battery_percent = None;
            ctx.low_battery = false;
        }
    }

    discover_backlight(&mut ctx);

    let fx = Effectors {
        shadow,
        worker: worker_tx,
        queue: tx.clone(),
        manager: manager.clone(),
        session: session.clone(),
        powerd: PowerdProxy::new(&conn).await?,
        locked_rx,
    };

    // Initial world snapshot.
    ctx.lid_closed = manager_uncached.lid_closed().await.unwrap_or(false);
    ctx.logind_inhibitor = sources::logind_real_inhibitor_active(&manager)
        .await
        .unwrap_or(false);
    let (wayland_inh, _health) = hypridle_log::wayland_inhibitor_active();
    ctx.wayland_inhibitor = wayland_inh;
    ctx.ext_mon_count = hyprctl::ext_monitor_count(0).await;

    // Seed the active profile from the existing symlink so reconciles can
    // detect "no change" instead of forcing a spurious reload at startup.
    if let Some(name) = crate::sysio::profiles::active_profile_name() {
        if let Some(p) = crate::sysio::profiles::load_profiles()
            .into_iter()
            .find(|p| p.name == name)
        {
            ctx.edp_policy = p.edp;
        }
        ctx.current_profile = Some(name);
    }
    info!(
        "initial profile: {} (edp_policy={})",
        ctx.current_profile.as_deref().unwrap_or("None"),
        ctx.edp_policy.as_str()
    );
    // Align with reality (profiles edited offline / monitors changed while
    // down), then seed power policy AFTER docked-ness lands.
    let _ = tx.send(Event::MonitorsChanged).await;
    let _ = tx
        .send(Event::PowerOverrideChanged(sysfs::read_first_word(
            &paths::power_override_file(),
        )))
        .await;

    ctx.locked = match &session {
        Some(s) => match s.locked_hint().await {
            Ok(v) => v,
            Err(e) => {
                warn!("initial LockedHint read failed: {e}");
                hyprctl::hyprlock_running().await
            }
        },
        None => hyprctl::hyprlock_running().await,
    };
    let _ = locked_tx.send(ctx.locked);

    // Spawn everything; the dispatcher owns ctx from here.
    tokio::spawn(effectors::effector_worker(worker_rx));
    tokio::spawn(sources::hypr_socket_reader(tx.clone()));
    tokio::spawn(sources::inhibitor_poller(tx.clone(), manager.clone()));
    tokio::spawn(sources::mode_poller(tx.clone()));
    tokio::spawn(sources::reconcile_snapshot_task(
        tx.clone(),
        manager_uncached,
        ctx.ext_mon_count,
    ));
    tokio::spawn(sources::lid_watcher(tx.clone(), manager.clone()));
    tokio::spawn(sources::sleep_watcher(tx.clone(), manager.clone()));
    if let Some(s) = session {
        tokio::spawn(sources::lock_watcher(tx.clone(), locked_tx, s));
    }
    if let Some(up) = upower {
        tokio::spawn(sources::ac_watcher(tx.clone(), up));
    }
    if let Some((_, device)) = battery {
        tokio::spawn(sources::battery_watcher(tx.clone(), device));
    }
    tokio::spawn(sources::powerd_name_watcher(tx.clone(), conn.clone()));

    dispatcher::run(rx, ctx, fx).await;
    Ok(())
}
