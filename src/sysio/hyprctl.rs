//! Async hyprctl wrappers for the daemon (tokio::process — a slow or hung
//! hyprctl must never stall event dispatch; see the effector worker).

use serde::Deserialize;
use tracing::warn;

pub const EDP_MONITOR: &str = "eDP-2";

async fn hyprctl_json(args: &[&str]) -> Option<Vec<serde_json::Value>> {
    let value = hypr_ipc::hyprctl_json(args, hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    value.as_array().cloned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSnapshot {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default = "default_true")]
    pub dpms_status: bool,
}

fn default_true() -> bool {
    true
}

/// One `hyprctl -j monitors` payload. The reconciler needs three different
/// facts from it every pass; fetching it once and deriving them is the
/// difference between one subprocess per tick and three identical ones.
pub async fn monitors() -> Option<Vec<MonitorSnapshot>> {
    let value = hypr_ipc::hyprctl_json(&["-j", "monitors"], hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    serde_json::from_value(value).ok()
}

/// Connected non-eDP monitor count. Returns `prev` on hyprctl failure: a
/// transient hyprctl error must not look like an undock (it would expire
/// power overrides and flip profiles).
pub fn ext_monitor_count_in(monitors: Option<&[MonitorSnapshot]>, prev: u32) -> u32 {
    match monitors {
        Some(monitors) => monitors
            .iter()
            .filter(|m| !m.name.starts_with("eDP"))
            .count() as u32,
        None => {
            warn!("ext_monitor_count failed (keeping {prev})");
            prev
        }
    }
}

pub async fn ext_monitor_count(prev: u32) -> u32 {
    ext_monitor_count_in(monitors().await.as_deref(), prev)
}

/// Snapshot of currently-connected monitor descriptions.
pub async fn monitor_signature() -> Vec<String> {
    match monitors().await {
        Some(monitors) => monitors.into_iter().map(|m| m.description).collect(),
        None => {
            warn!("monitor_signature failed");
            Vec::new()
        }
    }
}

/// Run a mutating hyprctl command and require the literal `ok` reply.
///
/// hyprctl's exit code alone is unreliable: it is non-zero only when the
/// reply starts with `error:`, and several failures don't (e.g. `keyword`
/// rejected under the Lua config replies "keyword can't work with non-legacy
/// parsers. Use eval." with exit 0). Success is exactly `ok` on stdout.
pub async fn hyprctl_ok(args: &[&str]) -> bool {
    match hypr_ipc::hyprctl_ok(args, hypr_ipc::HYPRCTL_TIMEOUT).await {
        Ok(()) => true,
        Err(e) => {
            warn!("hyprctl {args:?} failed: {e}");
            false
        }
    }
}

/// Whether the eDP panel is disabled; None when undeterminable.
pub fn edp_is_disabled_in(monitors: &[MonitorSnapshot]) -> Option<bool> {
    monitors
        .iter()
        .find(|m| m.name == EDP_MONITOR)
        .map(|m| m.disabled)
}

pub async fn edp_is_disabled() -> Option<bool> {
    let value = hypr_ipc::hyprctl_json(&["monitors", "all", "-j"], hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    let monitors: Vec<MonitorSnapshot> = serde_json::from_value(value).ok()?;
    monitors
        .iter()
        .find(|m| m.name == EDP_MONITOR)
        .map(|m| m.disabled)
}

/// Whether any ENABLED output is currently DPMS off; None when
/// undeterminable. Disabled outputs are excluded: a disabled eDP reports
/// dpmsStatus false forever and would look like a permanent blank.
pub fn any_enabled_monitor_dpms_off_in(monitors: &[MonitorSnapshot]) -> bool {
    monitors.iter().any(|m| !m.disabled && !m.dpms_status)
}

/// Whether Hyprland currently holds ext-session-lock-v1. None when
/// undeterminable. Complements logind LockedHint: a stuck hint with no
/// compositor lock means the locker is dead.
pub async fn session_is_locked() -> Option<bool> {
    let out = hypr_ipc::hyprctl_output(&["locked"], hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Current cursor position; None when undeterminable. Reads fine while the
/// outputs are DPMS off — input keeps flowing to the compositor even when
/// nothing is lit, which is what makes this a usable presence signal.
pub async fn cursor_pos() -> Option<(i64, i64)> {
    let out = hypr_ipc::hyprctl_output(&["-j", "cursorpos"], hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((v.get("x")?.as_i64()?, v.get("y")?.as_i64()?))
}

/// IDs of the (regular) workspaces currently assigned to `monitor`. Used to
/// find workspaces stranded on a disabled eDP. Special workspaces (negative
/// ids) are excluded — they are monitor-local overlays, not switchable
/// targets. Empty on hyprctl failure or when none match.
pub async fn workspaces_on_monitor(monitor: &str) -> Vec<i64> {
    match hyprctl_json(&["workspaces", "-j"]).await {
        Some(workspaces) => workspaces
            .iter()
            .filter(|w| w.get("monitor").and_then(|m| m.as_str()) == Some(monitor))
            .filter_map(|w| w.get("id").and_then(|i| i.as_i64()))
            .filter(|id| *id > 0)
            .collect(),
        None => {
            warn!("workspaces_on_monitor({monitor}) failed");
            Vec::new()
        }
    }
}

/// Name of the first enabled non-eDP (external) monitor, or None when only
/// the eDP — or nothing — is enabled. Plain `monitors` (not `monitors all`)
/// lists enabled outputs only, which is exactly the set that can receive a
/// re-homed workspace.
pub async fn first_external_monitor() -> Option<String> {
    let monitors = hyprctl_json(&["-j", "monitors"]).await?;
    monitors.iter().find_map(|m| {
        let name = m.get("name").and_then(|n| n.as_str())?;
        (!name.starts_with("eDP")).then(|| name.to_string())
    })
}
