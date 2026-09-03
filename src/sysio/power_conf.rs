//! io wrapper for ~/.config/hypr/power.conf (missing file -> defaults).

use crate::paths;
use crate::pure::power::{
    DEFAULT_BATTERY_LOW_PCT, PowerConf, PowerPolicy, parse_power_conf, parse_power_policy,
};

pub fn load_power_policy() -> (PowerPolicy, u8) {
    match std::fs::read_to_string(paths::power_conf_file()) {
        Ok(text) => {
            let (policy, pct, warnings) = parse_power_policy(&text);
            for w in warnings {
                eprintln!("WARNING power.conf: {w}");
            }
            (policy, pct)
        }
        Err(_) => (PowerPolicy::default(), DEFAULT_BATTERY_LOW_PCT),
    }
}

/// Full power.conf, including lid presence. Missing file -> defaults
/// (present, so a laptop with no power.conf still takes the lid inhibitor).
pub fn load_power_conf() -> PowerConf {
    match std::fs::read_to_string(paths::power_conf_file()) {
        Ok(text) => {
            let (conf, warnings) = parse_power_conf(&text);
            for w in warnings {
                eprintln!("WARNING power.conf: {w}");
            }
            conf
        }
        Err(_) => PowerConf::default(),
    }
}
