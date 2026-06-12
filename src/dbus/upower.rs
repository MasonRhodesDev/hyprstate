//! org.freedesktop.UPower proxies (AC state + DisplayDevice battery).

use zbus::proxy;

pub const DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

#[proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
pub trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower"
)]
pub trait UPowerDevice {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;

    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;

    /// 2 = battery, 3 = ups.
    #[zbus(property, name = "Type")]
    fn device_type(&self) -> zbus::Result<u32>;
}
