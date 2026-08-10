//! Async hyprctl wrappers for the daemon (tokio::process — a slow or hung
//! hyprctl must never stall event dispatch; see the effector worker).

use tokio::process::Command;
use tracing::warn;

pub const EDP_MONITOR: &str = "eDP-2";

async fn hyprctl_json(args: &[&str]) -> Option<Vec<serde_json::Value>> {
    let out = Command::new("hyprctl").args(args).output().await.ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// One `hyprctl -j monitors` payload. The reconciler needs three different
/// facts from it every pass; fetching it once and deriving them is the
/// difference between one subprocess per tick and three identical ones.
pub async fn monitors() -> Option<Vec<serde_json::Value>> {
    hyprctl_json(&["-j", "monitors"]).await
}

/// Connected non-eDP monitor count. Returns `prev` on hyprctl failure: a
/// transient hyprctl error must not look like an undock (it would expire
/// power overrides and flip profiles).
pub fn ext_monitor_count_in(monitors: Option<&[serde_json::Value]>, prev: u32) -> u32 {
    match monitors {
        Some(monitors) => monitors
            .iter()
            .filter(|m| {
                !m.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .starts_with("eDP")
            })
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
    match hyprctl_json(&["-j", "monitors"]).await {
        Some(monitors) => monitors
            .iter()
            .map(|m| {
                m.get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
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
    let out = match Command::new("hyprctl").args(args).output().await {
        Ok(out) => out,
        Err(e) => {
            warn!("hyprctl failed to spawn: {args:?}: {e}");
            return false;
        }
    };
    let reply = String::from_utf8_lossy(&out.stdout);
    let reply = reply.trim();
    if !out.status.success() || reply != "ok" {
        warn!(
            "hyprctl {args:?} failed (rc={:?}): {}",
            out.status.code(),
            if reply.is_empty() {
                String::from_utf8_lossy(&out.stderr).trim().to_string()
            } else {
                reply.to_string()
            }
        );
        return false;
    }
    true
}

/// Whether the eDP panel is disabled; None when undeterminable.
pub fn edp_is_disabled_in(monitors: &[serde_json::Value]) -> Option<bool> {
    monitors
        .iter()
        .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(EDP_MONITOR))
        .map(|m| m.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false))
}

pub async fn edp_is_disabled() -> Option<bool> {
    let monitors = hyprctl_json(&["monitors", "all", "-j"]).await?;
    monitors
        .iter()
        .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(EDP_MONITOR))
        .map(|m| m.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false))
}

/// Whether any ENABLED output is currently DPMS off; None when
/// undeterminable. Disabled outputs are excluded: a disabled eDP reports
/// dpmsStatus false forever and would look like a permanent blank.
pub fn any_enabled_monitor_dpms_off_in(monitors: &[serde_json::Value]) -> bool {
    monitors.iter().any(|m| {
        !m.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false)
            && !m
                .get("dpmsStatus")
                .and_then(|d| d.as_bool())
                .unwrap_or(true)
    })
}

/// Current cursor position; None when undeterminable. Reads fine while the
/// outputs are DPMS off — input keeps flowing to the compositor even when
/// nothing is lit, which is what makes this a usable presence signal.
pub async fn cursor_pos() -> Option<(i64, i64)> {
    let out = Command::new("hyprctl")
        .args(["-j", "cursorpos"])
        .output()
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
