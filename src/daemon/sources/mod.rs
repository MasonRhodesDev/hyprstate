pub mod files;
pub mod inhibitors;
pub mod reconciler;
pub mod uevent;
pub mod wayland_idle;

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
use logind_session::{LogindManagerProxy, LogindSessionProxy};
pub use reconciler::ReconcileTrigger;

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
                    let ev = if args.start {
                        Event::Sleeping
                    } else {
                        Event::Resumed
                    };
                    if tx.send(ev).await.is_err() {
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
        .build()
        .await
        .ok()?;
    let uncached = LogindSessionProxy::builder(conn)
        .path(path.clone())
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
    let _ = stream.next().await;
    while let Some(change) = stream.next().await {
        if let Ok(v) = change.get().await {
            let _ = locked_tx.send(v);
            if tx.send(Event::LockChanged(v)).await.is_err() {
                return;
            }
        }
    }
}

pub async fn ac_watcher(tx: mpsc::Sender<Event>, upower: UPowerProxy<'static>) {
    let mut stream = upower.receive_on_battery_changed().await;
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
    let _ = stream.next().await;
    while let Some(change) = stream.next().await {
        if let Ok(pct) = change.get().await
            && tx.send(Event::BatteryPercent(pct)).await.is_err()
        {
            return;
        }
    }
}

pub async fn battery_initial(conn: &Connection) -> Option<(f64, UPowerDeviceProxy<'static>)> {
    let device = UPowerDeviceProxy::builder(conn)
        .path(DISPLAY_DEVICE_PATH)
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
    use crate::sysio::hyprctl::any_client_inhibiting_idle_in;

    fn row(what: &str, who: &str, mode: &str) -> (String, String, String, String, u32, u32) {
        (what.into(), who.into(), "why".into(), mode.into(), 1000, 1)
    }

    #[test]
    fn mason_air_boot_state_holds_no_claim() {
        let rows = [
            row("sleep", "hypridle", "delay"),
            row("sleep", "hypridle", "block"),
            row("handle-lid-switch", "hyprstate", "block"),
            row("sleep", "NetworkManager", "delay"),
        ];
        assert!(!logind_has_real_idle_block(&rows));
        let clients = serde_json::json!([
            {"class": "kitty", "inhibitingIdle": false},
            {"class": "firefox", "inhibitingIdle": false}
        ]);
        assert!(!any_client_inhibiting_idle_in(clients.as_array().unwrap()));
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
        let rows = [row("idle", "firefox", "block")];
        assert!(logind_has_real_idle_block(&rows));
    }
}
