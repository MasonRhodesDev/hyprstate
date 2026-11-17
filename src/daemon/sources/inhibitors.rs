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

pub async fn logind_inhibitor_watcher(
    tx: mpsc::Sender<Event>,
    conn: Connection,
    manager: LogindManagerProxy<'static>,
    mut last: bool,
) {
    loop {
        let stream = async {
            let props = PropertiesProxy::builder(&conn)
                .destination(LOGIN1)
                .path(logind_session::MANAGER_PATH)
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
