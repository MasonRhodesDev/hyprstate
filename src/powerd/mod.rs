pub mod knobs;

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use futures_util::StreamExt;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};
use zbus::object_server::SignalEmitter;

use crate::paths;
use crate::pure::power::PowerProfile;
use logind_session::LogindManagerProxy;

#[derive(zbus::DBusError, Debug)]
#[zbus(prefix = "org.hyprstate.Power1")]
enum PowerdError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InvalidProfile(String),
}

struct Power1 {
    active: StdMutex<PowerProfile>,
    latest: StdMutex<Option<PowerProfile>>,
    apply_lock: AsyncMutex<()>,
    aspm_writable: bool,
}

impl Power1 {
    fn active_profile_value(&self) -> PowerProfile {
        *self.active.lock().unwrap()
    }
}

#[zbus::interface(name = "org.hyprstate.Power1")]
impl Power1 {
    async fn apply_profile(
        &self,
        profile: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<HashMap<String, String>, PowerdError> {
        let Ok(parsed) = profile.parse::<PowerProfile>() else {
            return Err(PowerdError::InvalidProfile(format!(
                "profile must be one of power-saver|balanced|performance, got {profile:?}"
            )));
        };
        *self.latest.lock().unwrap() = Some(parsed);
        let _guard = self.apply_lock.lock().await;
        {
            let latest = self.latest.lock().unwrap();
            if *latest != Some(parsed) {
                let superseded = latest.map(|p| p.as_str()).unwrap_or("?");
                return Ok(HashMap::from([(
                    "coalesced".to_string(),
                    format!("superseded-by:{superseded}"),
                )]));
            }
        }
        let results = knobs::powerd_apply(parsed, self.aspm_writable);
        *self.active.lock().unwrap() = parsed;
        knobs::persist_profile(parsed);
        Self::profile_applied(&emitter, parsed.as_str()).await.ok();
        self.active_profile_changed(&emitter).await.ok();
        info!("applied {}: {:?}", parsed.as_str(), results);
        Ok(results)
    }

    async fn set_dgpu_awake(&self, awake: bool) -> HashMap<String, String> {
        let results = knobs::apply_dgpu_runtime_pm(awake);
        knobs::persist_dgpu_pin(awake);
        info!("set_dgpu_awake({awake}): {results:?}");
        results
    }

    fn get_profile(&self) -> String {
        self.active_profile_value().as_str().to_string()
    }

    fn get_knobs(&self) -> HashMap<String, String> {
        knobs::knob_snapshot()
    }

    #[zbus(signal)]
    async fn profile_applied(emitter: &SignalEmitter<'_>, profile: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn active_profile(&self) -> String {
        self.active_profile_value().as_str().to_string()
    }
}

pub async fn run(session_bus: bool) -> anyhow::Result<()> {
    let aspm_writable = knobs::aspm_writability_probe();
    let active = knobs::persisted_profile();

    let iface = Power1 {
        active: StdMutex::new(active),
        latest: StdMutex::new(None),
        apply_lock: AsyncMutex::new(()),
        aspm_writable,
    };

    let builder = if session_bus {
        zbus::connection::Builder::session()
    } else {
        zbus::connection::Builder::system()
    };
    let conn = builder
        .name(paths::POWERD_BUS)
        .serve_at(paths::POWERD_PATH, iface)
        .build()
        .await?;
    info!("powerd up as {}", paths::POWERD_BUS);

    info!(
        "startup apply {}: {:?}",
        active.as_str(),
        knobs::powerd_apply(active, aspm_writable)
    );

    let dgpu_pin = knobs::persisted_dgpu_pin();
    info!(
        "startup dgpu-pin {dgpu_pin}: {:?}",
        knobs::apply_dgpu_runtime_pm(dgpu_pin)
    );

    let iface_ref = conn
        .object_server()
        .interface::<_, Power1>(paths::POWERD_PATH)
        .await?;
    match LogindManagerProxy::new(&conn).await {
        Ok(logind) => match logind.receive_prepare_for_sleep().await {
            Ok(mut stream) => {
                while let Some(signal) = stream.next().await {
                    let Ok(args) = signal.args() else { continue };
                    if !args.start {
                        let active = iface_ref.get().await.active_profile_value();
                        info!("resume: re-applying {}", active.as_str());
                        knobs::powerd_apply(active, aspm_writable);
                        let pin = knobs::persisted_dgpu_pin();
                        info!("resume: re-applying dgpu-pin {pin}");
                        knobs::apply_dgpu_runtime_pm(pin);
                    }
                }
            }
            Err(e) => warn!("PrepareForSleep subscription failed: {e}"),
        },
        Err(e) => warn!("PrepareForSleep subscription failed: {e}"),
    }

    std::future::pending::<()>().await;
    Ok(())
}
