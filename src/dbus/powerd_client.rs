//! Client proxy for org.hyprstate.Power1 (powerd).

use std::collections::HashMap;

use zbus::proxy;

#[proxy(
    interface = "org.hyprstate.Power1",
    default_service = "org.hyprstate.Power1",
    default_path = "/org/hyprstate/Power1"
)]
pub trait Powerd {
    fn apply_profile(&self, profile: &str) -> zbus::Result<HashMap<String, String>>;

    fn set_dgpu_awake(&self, awake: bool) -> zbus::Result<HashMap<String, String>>;

    fn get_profile(&self) -> zbus::Result<String>;

    fn get_knobs(&self) -> zbus::Result<HashMap<String, String>>;

    #[zbus(signal)]
    fn profile_applied(&self, profile: String) -> zbus::Result<()>;

    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
}
