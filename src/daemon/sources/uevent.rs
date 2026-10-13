//! Kernel uevents (NETLINK_KOBJECT_UEVENT, multicast group 1) for the
//! `power_supply` and `backlight` subsystems.
//!
//! Group 1 is the kernel's own broadcast and needs no privilege (the socket
//! is created with NL_CFG_F_NONROOT_RECV). A message is
//! `action@devpath\0KEY=VALUE\0...`. The power_supply core raises a change
//! event whenever a supply's properties change, so the sysfs AC fallback
//! (UPower down) is read on that event instead of every 5 s; the backlight
//! class raises add/remove when a panel driver appears or goes, which is
//! when the brightness device must be re-discovered.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::ReconcileTrigger;
use crate::daemon::event::Event;
use crate::paths;
use crate::sysio::sysfs;

/// Kernel uevent multicast group (group 2 is udevd's processed stream,
/// which carries a libudev header this parser does not speak).
const KERNEL_GROUP: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub subsystem: String,
}

/// Parse one kernel-format uevent datagram. None for anything that is not
/// `action@devpath` followed by KEY=VALUE pairs (udevd's `libudev` framed
/// messages included).
pub fn parse_uevent(buf: &[u8]) -> Option<Uevent> {
    let mut parts = buf.split(|b| *b == 0).filter(|p| !p.is_empty());
    let header = std::str::from_utf8(parts.next()?).ok()?;
    let (action, devpath) = header.split_once('@')?;
    if action.is_empty() || !devpath.starts_with('/') {
        return None;
    }
    let mut subsystem = None;
    for part in parts {
        let Ok(kv) = std::str::from_utf8(part) else {
            continue;
        };
        if let Some(v) = kv.strip_prefix("SUBSYSTEM=") {
            subsystem = Some(v.to_string());
        }
    }
    Some(Uevent {
        action: action.to_string(),
        devpath: devpath.to_string(),
        subsystem: subsystem?,
    })
}

fn open_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain libc calls with checked return values; the sockaddr is
    // a fully initialised sockaddr_nl of the size passed.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0;
        addr.nl_groups = KERNEL_GROUP;
        let rc = libc::bind(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn recv(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: buf is a valid writable slice of the length passed.
    let n = unsafe {
        libc::recv(
            fd.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
            0,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Read kernel uevents for ever; reconnect after a socket error.
pub async fn uevent_task(tx: mpsc::Sender<Event>, triggers: mpsc::Sender<ReconcileTrigger>) {
    // The sysfs AC view as last reported, so a battery capacity change
    // (also a power_supply uevent, every few minutes on battery) is a read
    // and not an event.
    let mut last_ac = sysfs::on_ac_sysfs();
    loop {
        let fd = match open_socket().and_then(AsyncFd::new) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("uevent socket unavailable ({e}); retrying");
                tokio::time::sleep(paths::SOURCE_RETRY).await;
                continue;
            }
        };
        info!("listening for power_supply and backlight uevents");
        let mut buf = vec![0u8; 8192];
        'socket: loop {
            let mut guard = match fd.readable().await {
                Ok(g) => g,
                Err(e) => {
                    warn!("uevent socket readiness failed: {e}");
                    break 'socket;
                }
            };
            loop {
                match recv(fd.get_ref(), &mut buf) {
                    Ok(n) => {
                        if let Some(ev) = parse_uevent(&buf[..n])
                            && !handle(&ev, &tx, &mut last_ac).await
                        {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        guard.clear_ready();
                        break;
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                        // The kernel dropped events on us: anything could
                        // have moved, so let the reconciler look.
                        warn!("uevent socket overflowed; requesting a reconcile");
                        let _ = triggers.try_send(ReconcileTrigger::UeventsLost);
                    }
                    Err(e) => {
                        warn!("uevent recv failed: {e}; reopening the socket");
                        break 'socket;
                    }
                }
            }
        }
        tokio::time::sleep(paths::SOURCE_RETRY).await;
    }
}

/// Route one uevent. Returns false when the dispatcher is gone.
async fn handle(ev: &Uevent, tx: &mpsc::Sender<Event>, last_ac: &mut Option<bool>) -> bool {
    match ev.subsystem.as_str() {
        "power_supply" => {
            let cur = sysfs::on_ac_sysfs();
            if cur == *last_ac {
                return true;
            }
            *last_ac = cur;
            let Some(on_ac) = cur else {
                return true;
            };
            debug!(
                "power_supply uevent {} {}: on_ac={on_ac}",
                ev.action, ev.devpath
            );
            // Same path as UPower's signal: the dispatcher drops a value it
            // already holds and the 5 s AC settle stays in charge.
            tx.send(Event::AcChanged(on_ac)).await.is_ok()
        }
        "backlight" if matches!(ev.action.as_str(), "add" | "remove") => {
            info!("backlight {} {}", ev.action, ev.devpath);
            tx.send(Event::BacklightHotplug).await.is_ok()
        }
        "leds"
            if matches!(ev.action.as_str(), "add" | "remove")
                && ev.devpath.contains("kbd_backlight") =>
        {
            info!("keyboard backlight LED {} {}", ev.action, ev.devpath);
            tx.send(Event::KbdLedHotplug).await.is_ok()
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_uevent;

    fn msg(parts: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            out.extend_from_slice(p.as_bytes());
            out.push(0);
        }
        out
    }

    /// An AC adapter going online, as the kernel broadcasts it.
    #[test]
    fn parses_a_power_supply_change() {
        let buf = msg(&[
            "change@/devices/platform/macsmc-power/power_supply/macsmc-ac",
            "ACTION=change",
            "DEVPATH=/devices/platform/macsmc-power/power_supply/macsmc-ac",
            "SUBSYSTEM=power_supply",
            "POWER_SUPPLY_NAME=macsmc-ac",
            "POWER_SUPPLY_ONLINE=1",
            "SEQNUM=4242",
        ]);
        let ev = parse_uevent(&buf).unwrap();
        assert_eq!(ev.action, "change");
        assert_eq!(ev.subsystem, "power_supply");
        assert!(ev.devpath.ends_with("/macsmc-ac"));
    }

    #[test]
    fn parses_a_backlight_add() {
        let buf = msg(&[
            "add@/devices/platform/soc/backlight/apple-panel-bl",
            "ACTION=add",
            "SUBSYSTEM=backlight",
        ]);
        let ev = parse_uevent(&buf).unwrap();
        assert_eq!(
            (ev.action.as_str(), ev.subsystem.as_str()),
            ("add", "backlight")
        );
    }

    /// udevd's processed stream (group 2) is framed with a libudev header;
    /// it must parse as nothing rather than as a bogus event.
    #[test]
    fn rejects_libudev_framing_and_junk() {
        let mut libudev = b"libudev\0".to_vec();
        libudev.extend_from_slice(&[0xfe, 0xed, 0xca, 0xfe, 0, 0, 0, 0]);
        assert_eq!(parse_uevent(&libudev), None);
        assert_eq!(
            parse_uevent(&msg(&["ACTION=change", "SUBSYSTEM=usb"])),
            None
        );
        assert_eq!(
            parse_uevent(&msg(&["change@/devices/x"])),
            None,
            "no SUBSYSTEM"
        );
        assert_eq!(parse_uevent(b""), None);
    }
}
