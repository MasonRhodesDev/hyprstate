//! `kbd status|set|up|down|auto`: the manual keys of the keyboard backlight
//! policy and its status view. The CLI never touches the LED; it writes the
//! two files the daemon watches (the persistent level, the runtime override
//! marker) and reads the daemon's status snapshot.

use std::fs;

use crate::daemon::kbd_policy::KbdStatus;
use crate::paths;
use crate::pure::kbd::{DEFAULT_KBD_LEVEL_PCT, clamp_level, step_level};
use crate::pure::power::KbdMode;
use crate::sysio::kbd::{find_als, find_kbd_led};
use crate::sysio::power_conf::load_power_conf;
use crate::sysio::sysfs::read_first_word;

fn stored_level() -> u8 {
    read_first_word(&paths::kbd_level_file())
        .and_then(|w| w.parse::<i64>().ok())
        .map(clamp_level)
        .unwrap_or(DEFAULT_KBD_LEVEL_PCT)
}

/// Write the level, then arm the override. In that order: the daemon reacts
/// to each file on its own, and the marker landing first would light the
/// old level for one event.
fn apply(level: u8) -> i32 {
    let file = paths::kbd_level_file();
    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&file, format!("{level}\n")) {
        eprintln!("kbd: level write failed: {e}");
        return 1;
    }
    match paths::kbd_override_file() {
        Some(marker) => {
            if let Err(e) = fs::write(&marker, "manual\n") {
                eprintln!("kbd: override marker write failed: {e}");
                return 1;
            }
        }
        None => eprintln!("kbd: no XDG_RUNTIME_DIR; level stored, override not armed"),
    }
    println!("{level}");
    0
}

pub fn run(action: &str, value: Option<&str>) -> i32 {
    match action {
        "set" => {
            let Some(v) = value else {
                eprintln!("kbd set needs a percent (0..100)");
                return 2;
            };
            let Ok(pct) = v.trim_end_matches('%').parse::<i64>() else {
                eprintln!("kbd set: not a percent: {v:?}");
                return 2;
            };
            apply(clamp_level(pct))
        }
        "up" => apply(step_level(stored_level(), true)),
        "down" => apply(step_level(stored_level(), false)),
        "auto" => {
            if let Some(marker) = paths::kbd_override_file()
                && let Err(e) = fs::remove_file(&marker)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("kbd: override marker remove failed: {e}");
                return 1;
            }
            println!("override cleared; automatic policy");
            0
        }
        "status" => {
            let conf = load_power_conf();
            let led = find_kbd_led();
            let als = find_als();
            let armed = paths::kbd_override_file().is_some_and(|p| p.exists());
            println!("level   : {}% (stored)", stored_level());
            println!("override: {}", if armed { "armed" } else { "no" });
            let policy = match (conf.kbd, &led, &als) {
                (KbdMode::Off, _, _) => "off (power.conf kbd = off)".to_string(),
                (_, None, _) => "inert: no kbd_backlight LED under /sys/class/leds".to_string(),
                (_, _, None) => "inert: no illuminance sensor under /sys/bus/iio".to_string(),
                (_, Some((name, max)), Some(attr)) => {
                    format!("auto (LED {name}, max {max}; sensor {})", attr.display())
                }
            };
            println!("policy  : {policy}");
            println!(
                "config  : kbd-idle {} s, kbd-dark-lux {}",
                conf.kbd_idle_secs, conf.kbd_dark_lux
            );
            let snapshot = paths::kbd_status_file()
                .and_then(|p| fs::read_to_string(p).ok())
                .and_then(|s| serde_json::from_str::<KbdStatus>(&s).ok());
            match snapshot {
                Some(s) => {
                    println!(
                        "daemon  : keys {}, typing {}, dark {} (last lux {})",
                        if s.lit { "lit" } else { "off" },
                        s.typing,
                        s.dark,
                        s.lux
                            .map(|l| format!("{l:.1}"))
                            .unwrap_or_else(|| "unknown".into())
                    );
                }
                None => println!(
                    "daemon  : unknown (no status snapshot; daemon not running or policy inert)"
                ),
            }
            0
        }
        other => {
            eprintln!("kbd: unknown action {other:?}");
            2
        }
    }
}
