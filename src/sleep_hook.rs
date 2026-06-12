//! `sleep-hook pre|post` — run as root from /usr/lib/systemd/system-sleep/.
//! Maintains /sys/.../power/wakeup = "enabled" on USB hubs and the tracked
//! input devices, pre-suspend and post-resume (the kernel can reset wakeup
//! state across s2idle).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::paths;

struct HookLog {
    file: Option<fs::File>,
}

impl HookLog {
    fn open() -> Self {
        let path = Path::new(paths::SLEEP_HOOK_LOG);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => HookLog { file: Some(f) },
            Err(e) => {
                eprintln!("hyprstate sleep-hook: cannot open log: {e}");
                HookLog { file: None }
            }
        }
    }

    fn line(&mut self, msg: &str) {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("[{stamp}] {msg}\n");
        match &mut self.file {
            Some(f) => {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
            None => eprint!("{line}"),
        }
    }
}

fn write_enabled(path: &Path, log: &mut HookLog) -> bool {
    match fs::write(path, "enabled") {
        Ok(()) => true,
        Err(e) => {
            log.line(&format!("  ! {}: {e}", path.display()));
            false
        }
    }
}

fn usb_devices() -> Vec<PathBuf> {
    fs::read_dir("/sys/bus/usb/devices")
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

pub fn run(action: &str) -> i32 {
    if action != "pre" && action != "post" {
        // systemd-suspend may fire other actions; ignore.
        return 0;
    }
    let mut log = HookLog::open();
    let label = if action == "pre" {
        "PRE-SUSPEND"
    } else {
        "POST-RESUME"
    };
    log.line(&format!("=== {label}: enabling USB wake ==="));

    // USB controller (PCI device).
    let ctrl = Path::new(paths::WAKE_USB_CONTROLLER);
    if ctrl.exists() {
        let ok = write_enabled(ctrl, &mut log);
        log.line(&format!(
            "  controller: {}",
            if ok { "enabled" } else { "FAILED" }
        ));
    }

    // USB root hubs.
    let hubs: Vec<PathBuf> = usb_devices()
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("usb"))
        })
        .map(|p| p.join("power/wakeup"))
        .filter(|p| p.exists())
        .collect();
    let enabled = hubs.iter().filter(|h| write_enabled(h, &mut log)).count();
    log.line(&format!("  root hubs: {enabled}/{} enabled", hubs.len()));

    // Intermediate hubs (devices whose product field contains "Hub").
    let mut intermediate = 0;
    for dev in usb_devices() {
        let Ok(product) = fs::read_to_string(dev.join("product")) else {
            continue;
        };
        if product.contains("Hub") {
            let wake = dev.join("power/wakeup");
            if wake.exists() && write_enabled(&wake, &mut log) {
                intermediate += 1;
            }
        }
    }
    log.line(&format!("  intermediate hubs: {intermediate} enabled"));

    // Specific input devices.
    for dev in usb_devices() {
        let Ok(vendor) = fs::read_to_string(dev.join("idVendor")) else {
            continue;
        };
        let Ok(product) = fs::read_to_string(dev.join("idProduct")) else {
            continue;
        };
        let (vendor, product) = (vendor.trim(), product.trim());
        for ((v, p), name) in paths::WAKE_USB_VENDORS {
            if (vendor, product) == (v, p) {
                let wake = dev.join("power/wakeup");
                if wake.exists() {
                    let ok = write_enabled(&wake, &mut log);
                    log.line(&format!(
                        "  {name}: {}",
                        if ok { "enabled" } else { "FAILED" }
                    ));
                }
            }
        }
    }

    log.line(&format!("=== {label} complete ==="));
    0
}
