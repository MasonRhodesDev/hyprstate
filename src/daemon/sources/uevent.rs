use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::ReconcileTrigger;
use crate::daemon::event::Event;
use crate::paths;
use crate::sysio::sysfs;

const KERNEL_GROUP: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub subsystem: String,
}

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

pub async fn uevent_task(tx: mpsc::Sender<Event>, triggers: mpsc::Sender<ReconcileTrigger>) {
    let mut last_ac = sysfs::on_ac_sysfs();
    loop {
        let registered =
            open_socket().and_then(|fd| unsafe { AsyncFd::register(fd) }.map_err(io::Error::from));
        let fd = match registered {
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
