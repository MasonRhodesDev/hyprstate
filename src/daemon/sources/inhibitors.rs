//! Keep-awake claim sensing from two real sources and no log text.
//!
//! - logind idle/sleep block inhibitors: `ListInhibitors`, re-read when the
//!   Manager's `BlockInhibited`/`DelayInhibited` properties change (logind
//!   emits that on every inhibitor start and stop).
//! - Wayland idle-inhibit surfaces: `hyprctl clients` `inhibitingIdle`, the
//!   compositor's own view. There is no event for a client dropping its
//!   inhibitor, so it is sampled at the decisions that need it (lid close, a
//!   suspend request, a reconcile pass) and, while Deferred is waiting on a
//!   claim to release, every `DEFERRED_CLAIM_POLL` until it does.
//!
//! The previous source was hypridle's journal, grepped for "inhibit locks"
//! every 2 s. On mason-air (hyprstate 2.7.4, hypr-DE 0.2.36) the flag was
//! true from boot with no claim anywhere: hypridle with `inhibit_sleep=3`
//! logs "Sleep inhibition enabled" and "Inhibited sleep with fd N" at
//! startup while holding a logind *sleep delay* inhibitor, and the tail
//! read with `--reverse` picked the oldest of the last 20 matches, so a
//! stale count stuck. Every lid close went LID_OPEN to DEFERRED and the
//! laptop never suspended.

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::fdo::PropertiesProxy;

use super::logind_real_inhibitor_active;
use crate::daemon::event::Event;
use crate::paths;
use crate::sysio::hyprctl;
use logind_session::LogindManagerProxy;

const LOGIN1: &str = "org.freedesktop.login1";
const LOGIN1_MANAGER: &str = "org.freedesktop.login1.Manager";

/// Re-read `ListInhibitors` whenever logind says its inhibitor set moved;
/// send only on an edge of the derived "real idle block" boolean.
pub async fn logind_inhibitor_watcher(
    tx: mpsc::Sender<Event>,
    conn: Connection,
    manager: LogindManagerProxy<'static>,
    mut last: bool,
) {
    loop {
        let stream = async {
            let props = PropertiesProxy::builder(&conn)
                .destination(LOGIN1)?
                .path(logind_session::MANAGER_PATH)?
                .build()
                .await?;
            props.receive_properties_changed().await
        }
        .await;
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!("logind BlockInhibited subscription failed: {e}; retrying");
                tokio::time::sleep(paths::SOURCE_RETRY).await;
                continue;
            }
        };
        info!("subscribed to logind inhibitor changes (BlockInhibited)");
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.interface_name.as_str() != LOGIN1_MANAGER {
                continue;
            }
            let touched = ["BlockInhibited", "DelayInhibited"].iter().any(|p| {
                args.changed_properties.contains_key(*p)
                    || args.invalidated_properties.iter().any(|i| i == p)
            });
            if !touched {
                continue;
            }
            let Some(cur) = logind_real_inhibitor_active(&manager).await else {
                continue;
            };
            if cur == last {
                continue;
            }
            last = cur;
            if tx
                .send(Event::Inhibitor {
                    wayland: false,
                    active: cur,
                })
                .await
                .is_err()
            {
                return;
            }
        }
        warn!("logind PropertiesChanged stream ended; resubscribing");
        tokio::time::sleep(paths::SOURCE_RETRY).await;
    }
}

/// Deferred holds a Wayland claim and waits for it to release. Nothing
/// announces that, so this is the daemon's one sampler: it runs only while
/// Deferred is on a Wayland claim and ends itself the moment the claim is
/// gone (or the state changes and the effector aborts it).
pub async fn wayland_claim_watch(tx: mpsc::Sender<Event>) {
    loop {
        tokio::time::sleep(paths::DEFERRED_CLAIM_POLL).await;
        match hyprctl::wayland_idle_inhibited().await {
            Some(false) => {
                let _ = tx
                    .send(Event::Inhibitor {
                        wayland: true,
                        active: false,
                    })
                    .await;
                return;
            }
            Some(true) => {}
            None => debug!("claim watch: hyprctl unavailable; keeping the claim"),
        }
    }
}
