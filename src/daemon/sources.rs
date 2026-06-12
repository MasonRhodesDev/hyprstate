//! Event source tasks. Each holds an mpsc Sender<Event> clone and never
//! touches Context — the dispatcher owns it.

use std::path::PathBuf;

use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use zbus::Connection;

use super::event::{Event, ReconcileSnapshot};
use crate::dbus::logind::{LogindManagerProxy, LogindSessionProxy};
use crate::dbus::upower::{DISPLAY_DEVICE_PATH, UPowerDeviceProxy, UPowerProxy};
use crate::paths;
use crate::sysio::{hyprctl, hypridle_log, sysfs};

// =========================================================================
// Hyprland socket2 reader
// =========================================================================

/// Resolve the socket2 path: env signature first; when that is stale (the
/// compositor restarted under us), rescan $XDG_RUNTIME_DIR/hypr/*/ for a
/// live Hyprland lock.
fn socket2_path() -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let candidate = runtime.join("hypr").join(&sig).join(".socket2.sock");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Rescan: any instance dir whose lock holds a live Hyprland PID.
    let hypr = runtime.join("hypr");
    for entry in std::fs::read_dir(&hypr).ok()?.flatten() {
        let lock = entry.path().join("hyprland.lock");
        let Ok(text) = std::fs::read_to_string(&lock) else {
            continue;
        };
        let Some(pid) = text
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() == "Hyprland" {
            let candidate = entry.path().join(".socket2.sock");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

pub async fn hypr_socket_reader(tx: mpsc::Sender<Event>) {
    loop {
        let Some(path) = socket2_path() else {
            warn!("no Hyprland event socket found; retrying in 2s");
            tokio::time::sleep(paths::INHIBIT_POLL).await;
            continue;
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
        tokio::time::sleep(paths::INHIBIT_POLL).await;
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
    for (who, _why, what, mode, _uid, _pid) in rows {
        if mode != "block" {
            continue;
        }
        let cats: Vec<&str> = what.split(':').collect();
        if !cats.contains(&"idle") && !cats.contains(&"sleep") {
            continue;
        }
        if paths::INHIBIT_BASELINE_WHO.contains(&who.as_str()) {
            continue;
        }
        return Some(true);
    }
    Some(false)
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

/// Poll platform_profile + the gpu/power override files; queue on change.
pub async fn mode_poller(tx: mpsc::Sender<Event>) {
    let mut last_platform = sysfs::read_first_word(paths::platform_profile_path());
    let mut last_gpu = sysfs::read_first_word(&paths::gpu_override_file());
    let mut last_power = sysfs::read_first_word(&paths::power_override_file());
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
    }
}

/// Gather the world every RECONCILE_INTERVAL; the dispatcher diffs/repairs.
/// `manager` must be an UNCACHED proxy: catching missed PropertiesChanged
/// signals is this task's whole purpose.
pub async fn reconcile_snapshot_task(
    tx: mpsc::Sender<Event>,
    manager: LogindManagerProxy<'static>,
    mut ext_prev: u32,
) {
    loop {
        tokio::time::sleep(paths::RECONCILE_INTERVAL).await;
        let lid = match manager.lid_closed().await {
            Ok(v) => v,
            Err(e) => {
                warn!("reconciler snapshot failed: {e}");
                continue;
            }
        };
        let ext = hyprctl::ext_monitor_count(ext_prev).await;
        ext_prev = ext;
        let logind_inh = logind_real_inhibitor_active(&manager)
            .await
            .unwrap_or(false);
        let (wayland_inh, _health) = hypridle_log::wayland_inhibitor_active();
        let locked = hyprctl::hyprlock_running().await;
        let on_ac = on_ac_sysfs();
        let edp_disabled = hyprctl::edp_is_disabled().await;
        let snap = ReconcileSnapshot {
            lid_closed: lid,
            ext_mon_count: ext,
            logind_inhibitor: logind_inh,
            wayland_inhibitor: wayland_inh,
            locked,
            on_ac,
            edp_disabled,
        };
        if tx.send(Event::ReconcileTick(Box::new(snap))).await.is_err() {
            return;
        }
    }
}

/// on_ac from /sys/class/power_supply/A*/online — UPower fallback. None on
/// a desktop / no AC supply.
pub fn on_ac_sysfs() -> Option<bool> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with('A') {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path().join("online")) {
            return Some(text.trim() == "1");
        }
    }
    None
}

// =========================================================================
// D-Bus subscriptions
// =========================================================================

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
    let Ok(mut stream) = manager.receive_prepare_for_sleep().await else {
        warn!("PrepareForSleep subscription failed");
        return;
    };
    while let Some(signal) = stream.next().await {
        let Ok(args) = signal.args() else { continue };
        if !args.start && tx.send(Event::Resumed).await.is_err() {
            return;
        }
    }
}

/// Resolve our graphical session: GetSessionByPID(0), falling back to
/// ListSessions with v1's scoring (class=user +1, graphical type +2,
/// active +4 / online +1).
pub async fn resolve_session(
    conn: &Connection,
    manager: &LogindManagerProxy<'static>,
) -> Option<LogindSessionProxy<'static>> {
    let path = match manager.get_session_by_pid(0).await {
        Ok(p) => Some(p),
        Err(e) => {
            warn!("GetSessionByPID(0) failed: {e} — falling back to ListSessions");
            let sessions = match manager.list_sessions().await {
                Ok(s) => s,
                Err(e2) => {
                    warn!("ListSessions fallback failed: {e2}");
                    return None;
                }
            };
            let uid = unsafe { libc::getuid() };
            let mut best: Option<(i32, zbus::zvariant::OwnedObjectPath)> = None;
            for (_id, suid, _user, _seat, path) in sessions {
                if suid != uid {
                    continue;
                }
                let Ok(proxy) = LogindSessionProxy::builder(conn)
                    .path(path.clone())
                    .ok()?
                    .build()
                    .await
                else {
                    continue;
                };
                let (Ok(state), Ok(class), Ok(stype)) = (
                    proxy.state().await,
                    proxy.class().await,
                    proxy.session_type().await,
                ) else {
                    continue;
                };
                let mut score = 0;
                if class == "user" {
                    score += 1;
                }
                if matches!(stype.as_str(), "wayland" | "x11" | "mir") {
                    score += 2;
                }
                if state == "active" {
                    score += 4;
                } else if state == "online" {
                    score += 1;
                }
                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, path));
                }
            }
            best.map(|(_, p)| p)
        }
    };
    let Some(path) = path else {
        warn!("no logind session resolved — lock detection via LockedHint disabled");
        return None;
    };
    let proxy = LogindSessionProxy::builder(conn)
        .path(path.clone())
        .ok()?
        .build()
        .await
        .ok()?;
    info!(
        "subscribed to session {} for LockedHint changes",
        path.as_str()
    );
    Some(proxy)
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
