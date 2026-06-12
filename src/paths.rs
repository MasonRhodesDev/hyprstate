//! Every filesystem path and tunable constant in one place (port of
//! hyprstate.py's constants block). Functions return fresh PathBufs; nothing
//! here touches the filesystem.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PLATFORM_PROFILE_PATH: &str = "/sys/firmware/acpi/platform_profile";
pub const DRI_BY_PATH: &str = "/dev/dri/by-path";
pub const LID_STATE_GLOB_DIR: &str = "/proc/acpi/button/lid";

/// Settle retry delay for `gpu select` on a bailed-transient (docked cold
/// boot where DP links aren't up at early-login sysfs read).
pub const GPU_SETTLE: Duration = Duration::from_millis(500);

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn hypr_config(file: &str) -> PathBuf {
    home().join(".config/hypr").join(file)
}

/// Runtime user state (deliberately not chezmoi-managed): manual GPU mode
/// override, first word igpu|dgpu|off|auto.
pub fn gpu_override_file() -> PathBuf {
    hypr_config("gpu-select")
}

/// Breadcrumb the daemon writes with the matched profile's `#@ gpu` value so
/// next login's `gpu select` (pre-compositor, can't match profiles) computes
/// from the same inputs.
pub fn gpu_breadcrumb_file() -> PathBuf {
    hypr_config("gpu-profile")
}

/// Contract between `gpu select` (run by uwsm pre-compositor) and the
/// daemon's drift detection. Schema v1, see GPU_SPEC.md.
pub fn gpu_state_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime.join("hypr-gpu-primary.json")
}

pub fn platform_profile_path() -> &'static Path {
    Path::new(PLATFORM_PROFILE_PATH)
}
