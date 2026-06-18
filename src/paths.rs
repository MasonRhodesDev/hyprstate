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

// ---- daemon timing ----

pub const GRACE_SECONDS: Duration = Duration::from_secs(30);
pub const DPMS_DELAY: Duration = Duration::from_secs(30);
pub const LOCK_WAIT: Duration = Duration::from_secs(2);
pub const INHIBIT_POLL: Duration = Duration::from_secs(2);
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// Coalesce monitor add/remove bursts before profile reconciliation.
pub const PROFILE_DEBOUNCE: Duration = Duration::from_millis(500);
/// AC plug-jiggle settle window before power policy reacts.
pub const POWER_AC_DEBOUNCE: Duration = Duration::from_secs(5);
pub const GPU_NOTIFY_MIN: Duration = Duration::from_secs(60);

/// logind inhibitor holders that do NOT count as "a real inhibitor is
/// active" (baseline daemons + our own).
pub const INHIBIT_BASELINE_WHO: [&str; 8] = [
    "ModemManager",
    "NetworkManager",
    "UPower",
    "hypridle",
    "logind-idle-control",
    "hyprstate",
    "hypr-power", // transitional; predecessor name
    "hypr-fsm",   // transitional; earlier predecessor
];

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

// ---- monitor profiles ----

pub fn profiles_dir() -> PathBuf {
    hypr_config("profiles")
}

pub fn active_profile_link() -> PathBuf {
    profiles_dir().join(".active.conf")
}

// ---- power policy (user side) ----

pub fn power_conf_file() -> PathBuf {
    hypr_config("power.conf")
}

pub fn power_override_file() -> PathBuf {
    hypr_config("power-override")
}

// ---- sleep hook / usb wake ----

pub const SLEEP_HOOK_LOG: &str = "/var/log/hyprstate-sleep.log";
pub const WAKE_USB_CONTROLLER: &str = "/sys/bus/pci/devices/0000:0e:00.3/power/wakeup";
/// (idVendor, idProduct) -> label for the input devices the hook tracks.
pub const WAKE_USB_VENDORS: [((&str, &str), &str); 2] = [
    (("3297", "1977"), "ZSA Voyager (keyboard)"),
    (("046d", "c539"), "Logitech Lightspeed (mouse receiver)"),
];

// The PIXA i2c-HID touchpad wedges on resume: the device stays half-alive
// (button events pass through, motion is silently dropped) until re-enumerated.
// Unbind/bind on post-resume forces a clean udev remove/add so the compositor's
// libinput backend recreates the device instead of reusing the wedged one.
//
// Two sysfs markers, deliberately tracked separately (see rebind_touchpad):
//   I2C_DEVICES_DIR/<client>     — symlink iff the device is present on the bus
//   I2C_HID_DRIVER_DIR/<client>  — symlink iff it is currently *bound*
// The driver dir also holds the bind/unbind files; writing the client name to
// unbind then bind cycles it. A device can be present-but-unbound (e.g. a prior
// failed bind), which is exactly the state a bind should recover — so the skip
// guard keys on presence, never on bound-ness.
pub const I2C_HID_DRIVER_DIR: &str = "/sys/bus/i2c/drivers/i2c_hid_acpi";
pub const I2C_DEVICES_DIR: &str = "/sys/bus/i2c/devices";
pub const TOUCHPAD_I2C_CLIENT: &str = "i2c-PIXA3854:00";

// ---- powerd (root effector; see POWER_SPEC.md) ----

pub const POWERD_BUS: &str = "org.hyprstate.Power1";
pub const POWERD_PATH: &str = "/org/hyprstate/Power1";
pub const POWERD_STATE_FILE: &str = "/var/lib/hyprstate/profile";
pub const PLATFORM_PROFILE_CHOICES_PATH: &str = "/sys/firmware/acpi/platform_profile_choices";
pub const CPUFREQ_DIR: &str = "/sys/devices/system/cpu/cpufreq";
pub const ASPM_POLICY_PATH: &str = "/sys/module/pcie_aspm/parameters/policy";
pub const INTEL_NO_TURBO_PATH: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";
