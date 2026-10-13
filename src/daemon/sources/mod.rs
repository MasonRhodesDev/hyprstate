//! Event source tasks. Each holds an mpsc Sender<Event> clone and never
//! touches Context — the dispatcher owns it.

pub mod reconciler;

use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use zbus::Connection;
use zbus::proxy::CacheProperties;

use super::event::Event;
use crate::dbus::logind_signals::SessionSignalsProxy;
use crate::dbus::upower::{DISPLAY_DEVICE_PATH, UPowerDeviceProxy, UPowerProxy};
use crate::paths;
use crate::sysio::{hypridle_log, sysfs};
use logind_session::{LogindManagerProxy, LogindSessionProxy};
pub use reconciler::ReconcileTrigger;

// =========================================================================
// Hyprland socket2 reader
// =========================================================================

pub async fn hypr_socket_reader(tx: mpsc::Sender<Event>) {
    loop {
        let path = match hypr_ipc::socket2_path() {
            Ok(path) => path,
            Err(_) => {
                warn!("no Hyprland event socket found; retrying in 2s");
                tokio::time::sleep(hypr_ipc::RECONNECT).await;
                continue;
            }
        };
        match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => {
                info!("connected to Hyprland event socket {}", path.display());
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let ev = if let Some(rest) = line.strip_prefix("monitoraddedv2>>") {
                        // payload: id,name,description
                        let name = rest.split(',').nth(1).unwrap_or("").to_string();
                        Some(Event::MonitorHotplug { added: true, name })
                    } else if let Some(rest) = line.strip_prefix("monitorremoved>>") {
                        Some(Event::MonitorHotplug {
                            added: false,
                            name: rest.to_string(),
                        })
                    } else if line.starts_with("configreloaded") {
                        Some(Event::ConfigReloaded)
                    } else {
                        None
                    };
                    if let Some(ev) = ev
                        && tx.send(ev).await.is_err()
                    {
                        return;
                    }
                }
                warn!("hypr socket closed; reconnecting in 2s");
            }
            Err(e) => warn!("hypr socket unavailable ({e}); retrying in 2s"),
        }
        tokio::time::sleep(hypr_ipc::RECONNECT).await;
    }
}

// =========================================================================
// Pollers
// =========================================================================

pub async fn logind_real_inhibitor_active(manager: &LogindManagerProxy<'_>) -> Option<bool> {
    let rows = match manager.list_inhibitors().await {
        Ok(r) => r,
        Err(e) => {
            warn!("ListInhibitors failed: {e}");
            return None;
        }
    };
    Some(logind_has_real_idle_block(&rows))
}

/// `rows` are Login1.ListInhibitors tuples: (what, who, why, mode, uid, pid).
pub(crate) fn logind_has_real_idle_block(
    rows: &[(String, String, String, String, u32, u32)],
) -> bool {
    for (what, who, _why, mode, _uid, _pid) in rows {
        if mode != "block" {
            continue;
        }
        let idle_or_sleep = what.split(':').any(|c| c == "idle" || c == "sleep");
        if !idle_or_sleep {
            continue;
        }
        if paths::INHIBIT_BASELINE_WHO.contains(&who.as_str()) {
            continue;
        }
        return true;
    }
    false
}

pub async fn inhibitor_poller(tx: mpsc::Sender<Event>, manager: LogindManagerProxy<'static>) {
    let mut last_logind = false;
    let mut last_wayland = false;
    let mut last_health = hypridle_log::ParseHealth::Ok;
    loop {
        tokio::time::sleep(paths::INHIBIT_POLL).await;
        let cur_logind = logind_real_inhibitor_active(&manager)
            .await
            .unwrap_or(last_logind);
        let (cur_wayland, health) = hypridle_log::wayland_inhibitor_active();
        if health != last_health {
            warn!(
                "wayland inhibitor source health: {} -> {}",
                last_health.as_str(),
                health.as_str()
            );
            last_health = health;
        }
        if cur_logind != last_logind {
            last_logind = cur_logind;
            if tx
                .send(Event::Inhibitor {
                    wayland: false,
                    active: cur_logind,
                })
                .await
                .is_err()
            {
                return;
            }
        }
        if cur_wayland != last_wayland {
            last_wayland = cur_wayland;
            if tx
                .send(Event::Inhibitor {
                    wayland: true,
                    active: cur_wayland,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// Poll platform_profile + the gpu/power override files + monitor-profile
/// TOML dirs; queue on change.
pub async fn mode_poller(tx: mpsc::Sender<Event>) {
    let mut last_platform = sysfs::read_first_word(paths::platform_profile_path());
    let mut last_gpu = sysfs::read_first_word(&paths::gpu_override_file());
    let mut last_power = sysfs::read_first_word(&paths::power_override_file());
    // Existence, not content: every other reader (reconciler, TimerExpired
    // guard, CLI) keys on the file existing, and a zero-byte file must not
    // read as "standing" to one and "withdrawn" to another.
    let mut last_suspend = paths::suspend_request_standing();
    let mut last_profiles = crate::sysio::profiles::profiles_source_fingerprint();
    loop {
        tokio::time::sleep(paths::INHIBIT_POLL).await;
        let cur = sysfs::read_first_word(paths::platform_profile_path());
        if cur != last_platform {
            last_platform = cur.clone();
            if tx.send(Event::PlatformProfileChanged(cur)).await.is_err() {
                return;
            }
        }
        let cur = sysfs::read_first_word(&paths::gpu_override_file());
        if cur != last_gpu {
            last_gpu = cur.clone();
            if tx.send(Event::GpuOverrideChanged(cur)).await.is_err() {
                return;
            }
        }
        let cur = sysfs::read_first_word(&paths::power_override_file());
        if cur != last_power {
            last_power = cur.clone();
            if tx.send(Event::PowerOverrideChanged(cur)).await.is_err() {
                return;
            }
        }
        let cur = paths::suspend_request_standing();
        if cur != last_suspend {
            last_suspend = cur;
            let payload = cur.then(|| "idle".to_string());
            if tx
                .send(Event::SuspendRequestChanged(payload))
                .await
                .is_err()
            {
                return;
            }
        }
        let cur = crate::sysio::profiles::profiles_source_fingerprint();
        if cur != last_profiles {
            last_profiles = cur;
            if tx.send(Event::ProfilesChanged).await.is_err() {
                return;
            }
        }
    }
}

// =========================================================================
// D-Bus subscriptions
// =========================================================================

/// logind's Session `Lock`/`Unlock` signals are reconcile triggers: a lock
/// or unlock requested without input (`loginctl unlock-session` over SSH)
/// is exactly when the stuck-DPMS repair has work and no LockedHint edge
/// may follow quickly enough to prompt it.
pub async fn session_signal_watcher(
    triggers: mpsc::Sender<ReconcileTrigger>,
    proxy: SessionSignalsProxy<'static>,
) {
    let (lock, unlock) = match (proxy.receive_lock().await, proxy.receive_unlock().await) {
        (Ok(l), Ok(u)) => (l, u),
        (Err(e), _) | (_, Err(e)) => {
            warn!("session Lock/Unlock subscription failed: {e}");
            return;
        }
    };
    let mut lock = lock;
    let mut unlock = unlock;
    loop {
        let trigger = tokio::select! {
            Some(_) = lock.next() => ReconcileTrigger::SessionLock,
            Some(_) = unlock.next() => ReconcileTrigger::SessionUnlock,
            else => return,
        };
        if triggers.send(trigger).await.is_err() {
            return;
        }
    }
}

pub async fn lid_watcher(tx: mpsc::Sender<Event>, manager: LogindManagerProxy<'static>) {
    let mut stream = manager.receive_lid_closed_changed().await;
    // Skip the initial emission: zbus property streams yield the current
    // value on subscription; daemon_main already read it, and v1 callbacks
    // fired only on real changes.
    let _ = stream.next().await;
    while let Some(change) = stream.next().await {
        if let Ok(v) = change.get().await
            && tx.send(Event::Lid(v)).await.is_err()
        {
            return;
        }
    }
}

pub async fn sleep_watcher(tx: mpsc::Sender<Event>, manager: LogindManagerProxy<'static>) {
    loop {
        match manager.receive_prepare_for_sleep().await {
            Ok(mut stream) => {
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    if !args.start && tx.send(Event::Resumed).await.is_err() {
                        return;
                    }
                }
                warn!("PrepareForSleep stream ended; resubscribing");
            }
            Err(e) => warn!("PrepareForSleep subscription failed: {e}; retrying"),
        }
        tokio::time::sleep(paths::SOURCE_RETRY).await;
    }
}

/// Resolve our graphical session via logind-session. Returns a (cached,
/// uncached) proxy pair: lock_watcher needs the cached one (property-change
/// streams are fed from the cache), the reconciler needs the uncached one
/// (fresh reads). Lid/suspend/LockedHint policy stays in this daemon.
pub async fn resolve_session(
    conn: &Connection,
) -> Option<(LogindSessionProxy<'static>, LogindSessionProxy<'static>)> {
    let session = match logind_session::resolve_session(conn).await {
        Ok(session) => session,
        Err(e) => {
            warn!("no logind session resolved ({e}) — lock detection via LockedHint disabled");
            return None;
        }
    };
    let path = session.path().clone();
    let cached = LogindSessionProxy::builder(conn)
        .path(path.clone())
        .ok()?
        .build()
        .await
        .ok()?;
    // The reconciler must read fresh values, not the signal-fed cache —
    // same reasoning as manager_uncached in daemon::run.
    let uncached = LogindSessionProxy::builder(conn)
        .path(path.clone())
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;
    info!(
        "subscribed to session {} for LockedHint changes",
        path.as_str()
    );
    Some((cached, uncached))
}

pub async fn lock_watcher(
    tx: mpsc::Sender<Event>,
    locked_tx: watch::Sender<bool>,
    session: LogindSessionProxy<'static>,
) {
    let mut stream = session.receive_locked_hint_changed().await;
    // Skip the initial emission: zbus property streams yield the current
    // value on subscription; daemon_main already read it, and v1 callbacks
    // fired only on real changes.
    let _ = stream.next().await;
    while let Some(change) = stream.next().await {
        if let Ok(v) = change.get().await {
            // Eager: the suspending tail's wait_for_lock watches this
            // channel; it must see the flip before the queue is drained.
            let _ = locked_tx.send(v);
            if tx.send(Event::LockChanged(v)).await.is_err() {
                return;
            }
        }
    }
}

pub async fn ac_watcher(tx: mpsc::Sender<Event>, upower: UPowerProxy<'static>) {
    let mut stream = upower.receive_on_battery_changed().await;
    // Skip the initial emission: zbus property streams yield the current
    // value on subscription; daemon_main already read it, and v1 callbacks
    // fired only on real changes.
    let _ = stream.next().await;
    while let Some(change) = stream.next().await {
        if let Ok(on_battery) = change.get().await
            && tx.send(Event::AcChanged(!on_battery)).await.is_err()
        {
            return;
        }
    }
}

pub async fn battery_watcher(tx: mpsc::Sender<Event>, device: UPowerDeviceProxy<'static>) {
    let mut stream = device.receive_percentage_changed().await;
    // Skip the initial emission: zbus property streams yield the current
    // value on subscription; daemon_main already read it, and v1 callbacks
    // fired only on real changes.
    let _ = stream.next().await;
    while let Some(change) = stream.next().await {
        if let Ok(pct) = change.get().await
            && tx.send(Event::BatteryPercent(pct)).await.is_err()
        {
            return;
        }
    }
}

/// Initial battery probe: (battery_percent, watcher device). None = no
/// battery / UPower down — battery machinery disabled. Desktops report
/// IsPresent=false with Percentage=0.0, which must not latch low_battery.
pub async fn battery_initial(conn: &Connection) -> Option<(f64, UPowerDeviceProxy<'static>)> {
    let device = UPowerDeviceProxy::builder(conn)
        .path(DISPLAY_DEVICE_PATH)
        .ok()?
        .build()
        .await
        .ok()?;
    let present = device.is_present().await.ok()?;
    let dtype = device.device_type().await.ok()?;
    if !present || !matches!(dtype, 2 | 3) {
        info!("no battery present — battery policy inputs disabled");
        return None;
    }
    let pct = device.percentage().await.ok()?;
    Some((pct, device))
}

/// Re-enable power applies when org.hyprstate.Power1 (re)appears.
pub async fn powerd_name_watcher(tx: mpsc::Sender<Event>, conn: Connection) {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
        warn!("DBus daemon interface unavailable");
        return;
    };
    let Ok(mut stream) = dbus.receive_name_owner_changed().await else {
        return;
    };
    while let Some(signal) = stream.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.name.as_str() == paths::POWERD_BUS
            && args.new_owner.is_some()
            && tx.send(Event::PowerdAppeared).await.is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::logind_has_real_idle_block;

    fn row(what: &str, who: &str, mode: &str) -> (String, String, String, String, u32, u32) {
        (what.into(), who.into(), "why".into(), mode.into(), 1000, 1)
    }

    #[test]
    fn counts_logind_idle_control_block() {
        let rows = [row("idle", "logind-idle-control", "block")];
        assert!(logind_has_real_idle_block(&rows));
    }

    #[test]
    fn ignores_hyprstate_baseline() {
        let rows = [row("handle-lid-switch", "hyprstate", "block")];
        assert!(!logind_has_real_idle_block(&rows));
    }

    #[test]
    fn what_who_order_not_swapped() {
        // Swapped fields would treat "idle" as who and miss the category check.
        let rows = [row("idle", "firefox", "block")];
        assert!(logind_has_real_idle_block(&rows));
    }
}
