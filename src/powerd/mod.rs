//! `hyprstate powerd`: root power-profile effector on the system bus
//! (org.hyprstate.Power1 — see POWER_SPEC.md). Mechanism only; the user
//! daemon owns policy.
//!
//! Success semantics: ApplyProfile success = the call completed. Per-row
//! results (written|unchanged|skipped-*|error:<msg>) are informational; an
//! all-skipped apply is still success (VM/desktop case). Calls arriving
//! while an apply is in flight update a latest-request slot; superseded
//! waiters return {"coalesced": "superseded-by:<profile>"}; only first and
//! latest apply.

pub mod knobs;

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use futures_util::StreamExt;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};
use zbus::object_server::SignalEmitter;

use crate::dbus::logind::LogindManagerProxy;
use crate::paths;
use crate::pure::power::PowerProfile;

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
    /// Apply a profile's knob whitelist. Coalesced under a click storm.
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

    /// Pin (`awake=true`) or release the discrete GPU's runtime PM. The
    /// daemon pushes `true` whenever the resolved GPU mode is `dgpu` so the
    /// active-renderer dGPU never autosuspends to D3cold (the FW16 DCN resume
    /// wedge). Persisted + re-applied on resume, like the power profile. No
    /// validation needed (the arg is a bool); never errors.
    async fn set_dgpu_awake(&self, awake: bool) -> HashMap<String, String> {
        let results = knobs::apply_dgpu_runtime_pm(awake);
        knobs::persist_dgpu_pin(awake);
        info!("set_dgpu_awake({awake}): {results:?}");
        results
    }

    /// Persisted active profile.
    fn get_profile(&self) -> String {
        self.active_profile_value().as_str().to_string()
    }

    /// Read-only live knob snapshot.
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
    // ASPM writability probe before anything else (root, sysfs).
    let aspm_writable = knobs::aspm_writability_probe();
    let active = knobs::persisted_profile();

    let iface = Power1 {
        active: StdMutex::new(active),
        latest: StdMutex::new(None),
        apply_lock: AsyncMutex::new(()),
        aspm_writable,
    };

    let builder = if session_bus {
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };
    let conn = builder
        .name(paths::POWERD_BUS)?
        .serve_at(paths::POWERD_PATH, iface)?
        .build()
        .await?;
    info!("powerd up as {}", paths::POWERD_BUS);

    // Initial apply of the persisted profile.
    info!(
        "startup apply {}: {:?}",
        active.as_str(),
        knobs::powerd_apply(active, aspm_writable)
    );

    // Initial apply of the persisted dgpu pin (the daemon re-pushes on its
    // own startup, but this re-asserts across a bare powerd restart).
    let dgpu_pin = knobs::persisted_dgpu_pin();
    info!(
        "startup dgpu-pin {dgpu_pin}: {:?}",
        knobs::apply_dgpu_runtime_pm(dgpu_pin)
    );

    // Re-apply on resume: firmware can reset EPP/boost across s2idle.
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
                        // The dGPU may have D3cold-resumed across s2idle and
                        // the kernel can reset power/control — re-assert the
                        // pin so dgpu mode stays wedge-proof.
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

    // Signal stream ended (or never started) — stay alive serving the bus.
    std::future::pending::<()>().await;
    Ok(())
}
