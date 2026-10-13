//! Keyboard backlight hardware: the `leds` class LED and the IIO ambient
//! light sensor. Discovery runs at startup and on a leds uevent; the sensor
//! is read only when the policy asks (a resume or a manual key press).

use std::fs;
use std::path::{Path, PathBuf};

const LEDS_DIR: &str = "/sys/class/leds";
const IIO_DIR: &str = "/sys/bus/iio/devices";

/// The keyboard backlight LED: `(name, max_brightness)`. Names carry
/// `kbd_backlight` on every driver seen so far (Asahi `kbd_backlight`,
/// ThinkPad `tpacpi::kbd_backlight`, Framework `framework_laptop::kbd_backlight`).
pub fn find_kbd_led() -> Option<(String, u32)> {
    let mut names: Vec<String> = fs::read_dir(LEDS_DIR)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let name = pick_kbd_led(&names)?.to_string();
    let max: u32 = fs::read_to_string(Path::new(LEDS_DIR).join(&name).join("max_brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (max > 0).then_some((name, max))
}

/// Pure half of `find_kbd_led`.
pub fn pick_kbd_led(names: &[String]) -> Option<&String> {
    names.iter().find(|n| n.contains("kbd_backlight"))
}

/// Current raw LED value, to seed the idempotence marker at startup.
pub fn led_brightness(name: &str) -> Option<u32> {
    fs::read_to_string(Path::new(LEDS_DIR).join(name).join("brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Illuminance attribute names, most to least preferred. `_input` is the
/// processed value in lux; `_raw` is the fallback on drivers without one.
const ALS_ATTRS: [&str; 3] = [
    "in_illuminance_input",
    "in_illuminance0_input",
    "in_illuminance_raw",
];

/// The ambient light sensor's illuminance attribute, if any IIO device has
/// one (mason-air: `iio:device0`, name `aop-sensors-als`).
pub fn find_als() -> Option<PathBuf> {
    let mut devices: Vec<PathBuf> = fs::read_dir(IIO_DIR)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    devices.sort();
    for dev in devices {
        let attrs: Vec<String> = fs::read_dir(&dev)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if let Some(attr) = pick_als_attr(&attrs) {
            return Some(dev.join(attr));
        }
    }
    None
}

/// Pure half of `find_als`.
pub fn pick_als_attr(attrs: &[String]) -> Option<&'static str> {
    ALS_ATTRS
        .iter()
        .copied()
        .find(|a| attrs.iter().any(|x| x == a))
}

/// One sensor reading. None when unreadable (the policy keeps its last
/// `dark` verdict).
pub fn read_lux(attr: &Path) -> Option<f64> {
    fs::read_to_string(attr).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{pick_als_attr, pick_kbd_led};

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picks_the_keyboard_led_among_others() {
        let names = v(&["input3::capslock", "kbd_backlight", "platform::mute"]);
        assert_eq!(pick_kbd_led(&names).unwrap(), "kbd_backlight");
        let tp = v(&["tpacpi::kbd_backlight", "tpacpi::power"]);
        assert_eq!(pick_kbd_led(&tp).unwrap(), "tpacpi::kbd_backlight");
        assert!(pick_kbd_led(&v(&["input3::capslock"])).is_none());
    }

    #[test]
    fn prefers_the_processed_illuminance_attribute() {
        let air = v(&["name", "in_illuminance_input", "uevent"]);
        assert_eq!(pick_als_attr(&air), Some("in_illuminance_input"));
        let raw_only = v(&["name", "in_illuminance_raw", "in_illuminance_scale"]);
        assert_eq!(pick_als_attr(&raw_only), Some("in_illuminance_raw"));
        assert_eq!(pick_als_attr(&v(&["name", "in_accel_x_raw"])), None);
    }
}
