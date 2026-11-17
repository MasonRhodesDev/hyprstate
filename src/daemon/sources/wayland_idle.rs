use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};

use crate::daemon::event::Event;
use crate::paths;

pub fn live_wayland_socket() -> Option<PathBuf> {
    let dir = hypr_ipc::instance_dir().ok()?;
    let lock = std::fs::read_to_string(dir.join(hypr_ipc::LOCK_NAME)).ok()?;
    socket_from_lock(&lock, dir.parent()?.parent()?)
}

pub fn socket_from_lock(lock: &str, runtime: &std::path::Path) -> Option<PathBuf> {
    let name = lock.lines().nth(1)?.trim();
    if name.is_empty() {
        return None;
    }
    let path = PathBuf::from(name);
    Some(if path.is_absolute() {
        path
    } else {
        runtime.join(path)
    })
}

#[derive(Default)]
struct State {
    pending: Vec<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.pending.push(true),
            ext_idle_notification_v1::Event::Resumed => state.pending.push(false),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ext_idle_notifier_v1::ExtIdleNotifierV1);

struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    fd: AsyncFd<OwnedFd>,
}

fn connect(timeout_ms: u32) -> Result<(Session, bool), String> {
    let conn = match live_wayland_socket() {
        Some(path) => {
            let stream = UnixStream::connect(&path)
                .map_err(|e| format!("connect {}: {e}", path.display()))?;
            Connection::from_socket(stream).map_err(|e| format!("wayland handshake: {e}"))?
        }
        None => Connection::connect_to_env().map_err(|e| format!("WAYLAND_DISPLAY: {e}"))?,
    };
    let (globals, queue): (GlobalList, EventQueue<State>) =
        registry_queue_init(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_seat: {e}"))?;
    let notifier: ext_idle_notifier_v1::ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| format!("ext_idle_notifier_v1: {e}"))?;
    let input_idle = notifier.version() >= 2;
    if input_idle {
        notifier.get_input_idle_notification(timeout_ms, &seat, &qh, ());
    } else {
        notifier.get_idle_notification(timeout_ms, &seat, &qh, ());
    }
    conn.flush().map_err(|e| format!("flush: {e}"))?;
    let fd = conn
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup socket: {e}"))?;
    let fd = unsafe { AsyncFd::register(fd) }.map_err(|e| format!("register socket: {e}"))?;
    Ok((
        Session {
            conn,
            queue,
            state: State::default(),
            fd,
        },
        input_idle,
    ))
}

pub async fn idle_notify_task(tx: mpsc::Sender<Event>, idle: Duration) {
    let timeout_ms = idle.as_millis().min(u32::MAX as u128) as u32;
    loop {
        let connected = tokio::task::spawn_blocking(move || connect(timeout_ms)).await;
        let (mut session, input_idle) = match connected {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                warn!("kbd: idle notification unavailable ({e}); retrying");
                tokio::time::sleep(paths::SOURCE_RETRY).await;
                continue;
            }
            Err(e) => {
                warn!("kbd: connect task failed ({e}); retrying");
                tokio::time::sleep(paths::SOURCE_RETRY).await;
                continue;
            }
        };
        if input_idle {
            info!(
                "kbd: typing from ext-idle-notify-v1 v2 (input idle, inhibitors ignored), timeout {} s",
                idle.as_secs()
            );
        } else {
            warn!(
                "kbd: compositor speaks ext-idle-notify-v1 v1 only; idle inhibitors will count as typing"
            );
        }
        if let Err(e) = run(&mut session, &tx).await {
            match e {
                Loop::Closed => return,
                Loop::Wayland(msg) => warn!("kbd: idle notification lost ({msg}); reconnecting"),
            }
        }
        tokio::time::sleep(paths::SOURCE_RETRY).await;
    }
}

enum Loop {
    Closed,
    Wayland(String),
}

async fn run(s: &mut Session, tx: &mpsc::Sender<Event>) -> Result<(), Loop> {
    loop {
        s.queue
            .dispatch_pending(&mut s.state)
            .map_err(|e| Loop::Wayland(format!("dispatch: {e}")))?;
        for idle in s.state.pending.drain(..) {
            debug!("kbd: {}", if idle { "idled" } else { "resumed" });
            if tx.send(Event::KbdIdle { idle }).await.is_err() {
                return Err(Loop::Closed);
            }
        }
        let Some(guard) = s.queue.prepare_read() else {
            continue;
        };
        s.conn
            .flush()
            .map_err(|e| Loop::Wayland(format!("flush: {e}")))?;
        let mut ready =
            s.fd.readable()
                .await
                .map_err(|e| Loop::Wayland(format!("poll: {e}")))?;
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                ready.clear_ready();
            }
            Err(e) => return Err(Loop::Wayland(format!("read: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::socket_from_lock;
    use std::path::Path;

    #[test]
    fn socket_name_comes_from_line_two() {
        let p = socket_from_lock("4242\nwayland-1\n", Path::new("/run/user/1000")).unwrap();
        assert_eq!(p, Path::new("/run/user/1000/wayland-1"));
        let abs = socket_from_lock("1\n/tmp/wl/wayland-9\n", Path::new("/run/user/1000")).unwrap();
        assert_eq!(abs, Path::new("/tmp/wl/wayland-9"));
    }

    #[test]
    fn pid_only_lock_has_no_socket() {
        assert_eq!(
            socket_from_lock("4242\n", Path::new("/run/user/1000")),
            None
        );
        assert_eq!(
            socket_from_lock("4242\n\n", Path::new("/run/user/1000")),
            None
        );
        assert_eq!(socket_from_lock("", Path::new("/run/user/1000")), None);
    }
}
