//! `power set|get|cycle|status [--waybar]` — override-file management and
//! powerd queries. The override file carries the profile only; the daemon
//! stamps the base state itself when it ingests the file.

use std::collections::HashMap;
use std::fs;

use crate::dbus::powerd_client::PowerdProxy;
use crate::paths;
use crate::pure::power::{PowerPolicy, PowerProfile};
use crate::sysio::power_conf::load_power_policy;
use crate::sysio::sysfs::read_first_word;

fn waybar_icon(profile: &str) -> &str {
    match profile {
        "power-saver" => "\u{f0fb6}",
        "balanced" => "\u{f0fb5}",
        "performance" => "\u{f04c5}",
        other => other,
    }
}

fn powerd_query() -> anyhow::Result<(String, HashMap<String, String>)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let conn = zbus::Connection::system().await?;
        let proxy = PowerdProxy::new(&conn).await?;
        Ok((proxy.get_profile().await?, proxy.get_knobs().await?))
    })
}

/// v1 prints the policy as a Python dict repr in `power get`; reproduced
/// verbatim so output stays diffable across the port.
fn policy_pydict(policy: &PowerPolicy) -> String {
    format!(
        "{{'docked-ac': '{}', 'ac': '{}', 'battery': '{}', 'battery-low': '{}'}}",
        policy.docked_ac.as_str(),
        policy.ac.as_str(),
        policy.battery.as_str(),
        policy.battery_low.as_str()
    )
}

pub fn run(action: &str, value: Option<&str>, waybar: bool) -> i32 {
    let override_word = read_first_word(&paths::power_override_file());
    let override_profile = override_word
        .as_deref()
        .filter(|w| w.parse::<PowerProfile>().is_ok());

    match action {
        "set" => {
            let Some(value) = value else {
                eprintln!("value must be auto|power-saver|balanced|performance");
                return 2;
            };
            if value == "auto" {
                let _ = fs::remove_file(paths::power_override_file());
                println!("override cleared — automatic policy");
                return 0;
            }
            if value.parse::<PowerProfile>().is_err() {
                eprintln!("value must be auto|power-saver|balanced|performance");
                return 2;
            }
            let file = paths::power_override_file();
            if let Some(parent) = file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::write(&file, format!("{value}\n")) {
                eprintln!("override write failed: {e}");
                return 1;
            }
            println!(
                "override: {value} (clears when AC state changes; \
                 `hyprstate power set auto` to clear now)"
            );
            0
        }
        "cycle" => {
            // auto -> power-saver -> balanced -> performance -> auto.
            let next = match override_profile {
                None => "power-saver",
                Some("power-saver") => "balanced",
                Some("balanced") => "performance",
                Some(_) => "auto",
            };
            run("set", Some(next), waybar)
        }
        "get" => {
            match override_profile {
                Some(p) => println!("override: {p}"),
                None => {
                    let (policy, _pct) = load_power_policy();
                    println!("auto (policy: {})", policy_pydict(&policy));
                }
            }
            0
        }
        "status" => {
            let (applied, knobs) = match powerd_query() {
                Ok(r) => r,
                Err(e) => {
                    if waybar {
                        println!(
                            "{}",
                            serde_json::json!({
                                "text": "⚡?",
                                "tooltip": "powerd unavailable",
                                "class": "unavailable",
                            })
                        );
                        return 0;
                    }
                    println!("powerd : unavailable ({e})");
                    if let Some(p) = override_profile {
                        println!("override: {p}");
                    }
                    return 0;
                }
            };
            if waybar {
                let mode = if override_profile.is_some() {
                    "override"
                } else {
                    "auto"
                };
                println!(
                    "{}",
                    serde_json::json!({
                        "text": waybar_icon(&applied),
                        "tooltip": format!("power: {applied} ({mode})"),
                        "class": format!("{mode} {applied}"),
                    })
                );
                return 0;
            }
            let suffix = if override_profile.is_some() {
                " (override)"
            } else {
                " (auto)"
            };
            println!("applied : {applied}{suffix}");
            let mut entries: Vec<_> = knobs.into_iter().collect();
            entries.sort();
            for (k, v) in entries {
                println!("  {k:<34} {v}");
            }
            0
        }
        other => {
            eprintln!("unknown action: {other}");
            2
        }
    }
}
