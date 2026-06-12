//! Async hyprctl wrappers for the daemon (tokio::process — a slow or hung
//! hyprctl must never stall event dispatch; see the effector worker).

use tokio::process::Command;
use tracing::warn;

pub const EDP_MONITOR: &str = "eDP-2";

async fn hyprctl_json(args: &[&str]) -> Option<Vec<serde_json::Value>> {
    let out = Command::new("hyprctl").args(args).output().await.ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// Connected non-eDP monitor count. Returns `prev` on hyprctl failure: a
/// transient hyprctl error must not look like an undock (it would expire
/// power overrides and flip profiles).
pub async fn ext_monitor_count(prev: u32) -> u32 {
    match hyprctl_json(&["-j", "monitors"]).await {
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

/// Whether the eDP panel is disabled; None when undeterminable.
pub async fn edp_is_disabled() -> Option<bool> {
    let monitors = hyprctl_json(&["monitors", "all", "-j"]).await?;
    monitors
        .iter()
        .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(EDP_MONITOR))
        .map(|m| m.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false))
}

pub async fn hyprlock_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "hyprlock"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
