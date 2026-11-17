use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PLATFORM_PROFILE_PATH: &str = "/sys/firmware/acpi/platform_profile";
pub const DRI_BY_PATH: &str = "/dev/dri/by-path";
pub const LID_STATE_GLOB_DIR: &str = "/proc/acpi/button/lid";

pub const GPU_SETTLE: Duration = Duration::from_millis(500);

pub const GRACE_SECONDS: Duration = Duration::from_secs(30);
pub const LOCK_WAIT: Duration = Duration::from_secs(2);
pub const SOURCE_RETRY: Duration = Duration::from_secs(2);
pub const SAFETY_TICK: Duration = Duration::from_secs(300);
pub const RECONCILE_SETTLE: Duration = Duration::from_secs(2);
pub const WAYBAR_POWER_SIGNAL: i32 = 9;
pub const DEFERRED_CLAIM_POLL: Duration = Duration::from_secs(5);
pub const PROFILE_DEBOUNCE: Duration = Duration::from_millis(500);
pub const POWER_AC_DEBOUNCE: Duration = Duration::from_secs(5);
pub const GPU_NOTIFY_MIN: Duration = Duration::from_secs(60);

pub const INHIBIT_BASELINE_WHO: [&str; 7] = [
    "ModemManager",
    "NetworkManager",
    "UPower",
    "hypridle",
    "hyprstate",
    "hypr-power",
    "hypr-fsm",
];

fn hypr_config(file: &str) -> PathBuf {
    match xdg_paths::ConfigDirs::from_env() {
        Ok(dirs) => dirs.config_dir("hypr").join(file),
        Err(_) => PathBuf::new(),
    }
}

pub fn gpu_override_file() -> PathBuf {
    hypr_config("gpu-select")
}

pub fn gpu_breadcrumb_file() -> PathBuf {
    hypr_config("gpu-profile")
}

fn xdg_runtime_dir() -> PathBuf {
    match xdg_paths::BaseDirs::from_env() {
        Ok(dirs) => dirs.runtime_dir().to_path_buf(),
        Err(_) => PathBuf::new(),
    }
}

pub fn gpu_state_path() -> PathBuf {
    xdg_runtime_dir().join("hypr-gpu-primary.json")
}

pub fn telemetry_sock_path() -> PathBuf {
    xdg_runtime_dir().join("hyprstate-telemetry.sock")
}

pub fn platform_profile_path() -> &'static Path {
    Path::new(PLATFORM_PROFILE_PATH)
}

pub const SYSTEM_PROFILES_DIR: &str = "/etc/monitor-profiles";

pub fn system_profiles_dir() -> &'static Path {
    Path::new(SYSTEM_PROFILES_DIR)
}

pub fn profiles_dir() -> PathBuf {
    hypr_config("profiles")
}

pub fn active_profile_link() -> PathBuf {
    profiles_dir().join(".active.lua")
}

pub fn edp_off_marker() -> PathBuf {
    hypr_config("edp-off")
}

pub fn power_conf_file() -> PathBuf {
    hypr_config("power.conf")
}

pub fn suspend_request_file() -> Option<PathBuf> {
    match xdg_paths::BaseDirs::from_env() {
        Ok(dirs) => Some(dirs.runtime_dir().join("hyprstate-suspend-request")),
        Err(_) => None,
    }
}

pub fn suspend_request_standing() -> bool {
    suspend_request_file().is_some_and(|p| p.exists())
}

pub fn power_override_file() -> PathBuf {
    hypr_config("power-override")
}

pub fn kbd_level_file() -> PathBuf {
    hypr_config("kbd-level")
}

pub fn kbd_override_file() -> Option<PathBuf> {
    match xdg_paths::BaseDirs::from_env() {
        Ok(dirs) => Some(dirs.runtime_dir().join("hyprstate-kbd-override")),
        Err(_) => None,
    }
}

pub fn kbd_status_file() -> Option<PathBuf> {
    match xdg_paths::BaseDirs::from_env() {
        Ok(dirs) => Some(dirs.runtime_dir().join("hyprstate").join("kbd-status.json")),
        Err(_) => None,
    }
}

pub const SLEEP_HOOK_LOG: &str = "/var/log/hyprstate-sleep.log";
pub const WAKE_USB_CONTROLLER: &str = "/sys/bus/pci/devices/0000:0e:00.3/power/wakeup";
pub const WAKE_USB_VENDORS: [((&str, &str), &str); 2] = [
    (("3297", "1977"), "ZSA Voyager (keyboard)"),
    (("046d", "c539"), "Logitech Lightspeed (mouse receiver)"),
];

pub const I2C_HID_DRIVER_DIR: &str = "/sys/bus/i2c/drivers/i2c_hid_acpi";
pub const I2C_DEVICES_DIR: &str = "/sys/bus/i2c/devices";
pub const TOUCHPAD_I2C_CLIENT: &str = "i2c-PIXA3854:00";

pub const POWERD_BUS: &str = "org.hyprstate.Power1";
pub const POWERD_PATH: &str = "/org/hyprstate/Power1";
pub const POWERD_STATE_FILE: &str = "/var/lib/hyprstate/profile";
pub const POWERD_DGPU_PIN_FILE: &str = "/var/lib/hyprstate/dgpu-pin";
pub const PLATFORM_PROFILE_CHOICES_PATH: &str = "/sys/firmware/acpi/platform_profile_choices";
pub const CPUFREQ_DIR: &str = "/sys/devices/system/cpu/cpufreq";
pub const ASPM_POLICY_PATH: &str = "/sys/module/pcie_aspm/parameters/policy";
pub const INTEL_NO_TURBO_PATH: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";
