//! The org.freedesktop.login1.Session `Lock` and `Unlock` signals. logind
//! emits them when a lock or unlock is REQUESTED (`loginctl lock-session`,
//! `Session.Lock()`), ahead of the locker flipping `LockedHint`. They are
//! reconcile triggers only: an unlock over SSH, for instance, leaves the
//! outputs dark with nobody to relight them except the stuck-DPMS repair.
//! logind-session 0.2.0 does not expose them, so the proxy lives here.

use zbus::proxy;

#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
pub trait SessionSignals {
    #[zbus(signal)]
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn unlock(&self) -> zbus::Result<()>;
}
