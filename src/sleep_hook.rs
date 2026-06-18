//! `sleep-hook pre|post` — run as root from /usr/lib/systemd/system-sleep/.
//! Maintains /sys/.../power/wakeup = "enabled" on USB hubs and the tracked
//! input devices, pre-suspend and post-resume (the kernel can reset wakeup
//! state across s2idle).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

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

    // Touchpad rebind: resume only, and only if currently bound (skip silently
    // if the module was physically detached). See paths::TOUCHPAD_I2C_CLIENT.
    if action == "post" {
        rebind_touchpad(&mut log);
    }

    log.line(&format!("=== {label} complete ==="));
    0
}

/// Number of bind attempts and the settle delay between them. The hook can run
/// before the i2c controller has finished resuming, so the first `bind` may
/// fail at descriptor read; retrying after a short delay lets resume catch up.
const REBIND_ATTEMPTS: u32 = 4;
const REBIND_BACKOFF: Duration = Duration::from_millis(300);

/// Force a clean re-enumeration of the PIXA i2c-HID touchpad by cycling it
/// through the driver's unbind/bind sysfs files. On resume the device often
/// comes back wedged (motion dropped, buttons fine); a clean rebind makes the
/// compositor's libinput recreate it.
///
/// Shape is dictated by two resume-specific failure modes. First, the hook can
/// run before the i2c controller has resumed, so an immediate `bind` may fail
/// at descriptor read — hence the bounded retry with a settle delay. Second,
/// `unbind` succeeding while `bind` fails leaves the device fully UNBOUND
/// (worse than wedged); so the skip guard keys on the device being absent from
/// the bus (never on un-bound-ness), we keep retrying `bind`, and we verify the
/// bound symlink actually returns rather than trusting the write's `Ok`. A
/// present-but-unbound device therefore self-heals on the next resume instead
/// of being skipped forever.
///
/// Both sysfs writes are synchronous (they block through remove()/probe()), so
/// no inter-write delay is needed; the backoff is only to let resume settle.
fn rebind_touchpad(log: &mut HookLog) {
    let driver = Path::new(paths::I2C_HID_DRIVER_DIR);
    let bound = driver.join(paths::TOUCHPAD_I2C_CLIENT);
    let present = Path::new(paths::I2C_DEVICES_DIR).join(paths::TOUCHPAD_I2C_CLIENT);

    if !present.exists() {
        // No such device on the bus: module physically detached, or a board
        // revision with a different touchpad. Nothing to recover.
        log.line("  touchpad rebind: skipped (device absent)");
        return;
    }

    for attempt in 1..=REBIND_ATTEMPTS {
        // Only unbind if currently bound; if a prior attempt left it unbound,
        // go straight to bind — that is the self-heal path.
        if bound.exists()
            && let Err(e) = fs::write(driver.join("unbind"), paths::TOUCHPAD_I2C_CLIENT)
        {
            log.line(&format!("  ! touchpad unbind: {e}"));
        }
        // `bind` blocks through the synchronous probe; success == Ok AND the
        // bound symlink reappeared (a probe can fail after an Ok-looking write).
        let res = fs::write(driver.join("bind"), paths::TOUCHPAD_I2C_CLIENT);
        if res.is_ok() && bound.exists() {
            if attempt == 1 {
                log.line("  touchpad rebind: ok");
            } else {
                log.line(&format!("  touchpad rebind: ok (attempt {attempt})"));
            }
            return;
        }
        log.line(&format!(
            "  touchpad rebind: attempt {attempt} failed ({res:?}); retrying"
        ));
        thread::sleep(REBIND_BACKOFF);
    }

    // Exhausted. Distinguish "still wedged" from the dangerous "left unbound"
    // (no touchpad until manual rebind/reboot) — the next resume will retry the
    // bind regardless, since the skip guard keys on presence, not bound-ness.
    if bound.exists() {
        log.line("  touchpad rebind: FAILED (still bound; may still be wedged)");
    } else {
        log.line(
            "  ! touchpad rebind: FAILED — device left UNBOUND; \
             recover with: echo -n i2c-PIXA3854:00 | sudo tee \
             /sys/bus/i2c/drivers/i2c_hid_acpi/bind",
        );
    }
}
