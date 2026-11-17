use serde::Deserialize;
use tracing::warn;

pub const EDP_MONITOR: &str = "eDP-2";

pub const FALLBACK_MONITOR: &str = "FALLBACK";

pub fn is_external(name: &str) -> bool {
    !name.starts_with("eDP") && name != FALLBACK_MONITOR
}

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

pub async fn monitors() -> Option<Vec<MonitorSnapshot>> {
    let value = hypr_ipc::hyprctl_json(&["-j", "monitors"], hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    serde_json::from_value(value).ok()
}

pub fn ext_monitor_count_in(monitors: Option<&[MonitorSnapshot]>, prev: u32) -> u32 {
    match monitors {
        Some(monitors) => monitors.iter().filter(|m| is_external(&m.name)).count() as u32,
        None => {
            warn!("ext_monitor_count failed (keeping {prev})");
            prev
        }
    }
}

pub async fn ext_monitor_count(prev: u32) -> u32 {
    ext_monitor_count_in(monitors().await.as_deref(), prev)
}

pub async fn monitor_signature() -> Vec<String> {
    match monitors().await {
        Some(monitors) => monitors.into_iter().map(|m| m.description).collect(),
        None => {
            warn!("monitor_signature failed");
            Vec::new()
        }
    }
}

pub async fn hyprctl_ok(args: &[&str]) -> bool {
    match hypr_ipc::hyprctl_ok(args, hypr_ipc::HYPRCTL_TIMEOUT).await {
        Ok(()) => true,
        Err(e) => {
            warn!("hyprctl {args:?} failed: {e}");
            false
        }
    }
}

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

pub fn any_enabled_monitor_dpms_off_in(monitors: &[MonitorSnapshot]) -> bool {
    monitors.iter().any(|m| !m.disabled && !m.dpms_status)
}

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

pub async fn cursor_pos() -> Option<(i64, i64)> {
    let out = hypr_ipc::hyprctl_output(&["-j", "cursorpos"], hypr_ipc::HYPRCTL_TIMEOUT)
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((v.get("x")?.as_i64()?, v.get("y")?.as_i64()?))
}

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

pub async fn first_external_monitor() -> Option<String> {
    let monitors = hyprctl_json(&["-j", "monitors"]).await?;
    monitors.iter().find_map(|m| {
        let name = m.get("name").and_then(|n| n.as_str())?;
        is_external(name).then(|| name.to_string())
    })
}

pub async fn wayland_idle_inhibited() -> Option<bool> {
    let clients = hyprctl_json(&["-j", "clients"]).await?;
    Some(any_client_inhibiting_idle_in(&clients))
}

pub fn any_client_inhibiting_idle_in(clients: &[serde_json::Value]) -> bool {
    clients
        .iter()
        .any(|c| c.get("inhibitingIdle").and_then(|v| v.as_bool()) == Some(true))
}

#[cfg(test)]
mod tests {
    use super::{MonitorSnapshot, any_client_inhibiting_idle_in, ext_monitor_count_in};

    fn monitors(names: &[&str]) -> Vec<MonitorSnapshot> {
        let json: Vec<_> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        serde_json::from_value(serde_json::Value::Array(json)).unwrap()
    }

    #[test]
    fn externals_exclude_the_panel() {
        let m = monitors(&["DP-1", "eDP-2", "DP-4"]);
        assert_eq!(ext_monitor_count_in(Some(&m), 0), 2);
    }

    #[test]
    fn the_fallback_placeholder_is_not_an_external() {
        let m = monitors(&["FALLBACK"]);
        assert_eq!(ext_monitor_count_in(Some(&m), 2), 0);
    }

    #[test]
    fn a_failed_query_keeps_the_previous_count() {
        assert_eq!(ext_monitor_count_in(None, 2), 2);
    }

    #[test]
    fn a_video_client_is_a_claim() {
        let clients = serde_json::json!([
            {"class": "kitty", "inhibitingIdle": false},
            {"class": "firefox", "inhibitingIdle": true}
        ]);
        assert!(any_client_inhibiting_idle_in(clients.as_array().unwrap()));
    }

    #[test]
    fn no_inhibiting_client_is_no_claim() {
        let clients = serde_json::json!([
            {"class": "kitty", "inhibitingIdle": false},
            {"class": "firefox"}
        ]);
        assert!(!any_client_inhibiting_idle_in(clients.as_array().unwrap()));
        assert!(!any_client_inhibiting_idle_in(&[]));
    }
}
