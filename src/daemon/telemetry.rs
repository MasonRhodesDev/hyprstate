//! Best-effort telemetry emitter over a Unix domain socket.
//!
//! After each relevant node/context change the daemon writes one
//! newline-delimited JSON frame to `$XDG_RUNTIME_DIR/hyprstate-telemetry.sock`.
//! The write is non-blocking and fire-and-forget: if no listener is connected
//! or the socket doesn't exist, the frame is silently dropped. This module
//! never affects FSM behavior.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::debug;

use super::ctx::Context;
use crate::pure::fsm::State;
use crate::pure::power::power_base_state;

/// A single telemetry frame for Help / observers.
#[derive(Debug, Clone, Serialize)]
pub struct TelemetryFrame {
    pub ts: u128,
    pub kind: &'static str,
    pub from: &'static str,
    pub event: &'static str,
    pub to: &'static str,
    pub screen: &'static str,
    pub ctx: FrameCtx,
    pub effectors: Vec<&'static str>,
}

/// Snapshot of world + power + display selection at emit time.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FrameCtx {
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub inhibitor: bool,
    pub locked: bool,
    pub on_ac: bool,
    pub low_battery: bool,
    pub power_base: String,
    pub desired_profile: String,
    pub applied_profile: String,
    pub active_profile: String,
}

impl FrameCtx {
    pub fn from_context(ctx: &Context) -> Self {
        let base = power_base_state(ctx.on_ac_settled, ctx.ext_mon_count, ctx.low_battery);
        let desired = ctx
            .power_override
            .unwrap_or_else(|| ctx.power_policy.for_base(base));
        Self {
            lid_closed: ctx.lid_closed,
            ext_mon_count: ctx.ext_mon_count,
            inhibitor: ctx.inhibitor(),
            locked: ctx.locked,
            on_ac: ctx.on_ac_settled,
            low_battery: ctx.low_battery,
            power_base: base.as_str().to_string(),
            desired_profile: desired.as_str().to_string(),
            applied_profile: ctx
                .power_applied
                .map(|p| p.as_str().to_string())
                .unwrap_or_default(),
            active_profile: ctx.current_profile.clone().unwrap_or_default(),
        }
    }

    fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.lid_closed.hash(&mut h);
        self.ext_mon_count.hash(&mut h);
        self.inhibitor.hash(&mut h);
        self.locked.hash(&mut h);
        self.on_ac.hash(&mut h);
        self.low_battery.hash(&mut h);
        self.power_base.hash(&mut h);
        self.desired_profile.hash(&mut h);
        self.applied_profile.hash(&mut h);
        self.active_profile.hash(&mut h);
        h.finish()
    }
}

/// Persistent emitter handle. Holds a lazy connection to the socket.
pub struct TelemetryEmitter {
    sock_path: PathBuf,
    stream: Option<UnixStream>,
    last_fp: Option<u64>,
}

impl TelemetryEmitter {
    pub fn new() -> Self {
        let runtime_dir =
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
        let sock_path = PathBuf::from(runtime_dir).join("hyprstate-telemetry.sock");
        Self {
            sock_path,
            stream: None,
            last_fp: None,
        }
    }

    /// Emit Help-relevant live state if the fingerprint changed.
    pub fn emit_help(
        &mut self,
        ctx: &Context,
        kind: &'static str,
        event: &'static str,
        from: State,
        to: State,
        effectors: Vec<&'static str>,
    ) {
        let frame = TelemetryFrame {
            ts: now_ms(),
            kind,
            from: from.as_str(),
            event,
            to: to.as_str(),
            screen: ctx.screen_state.as_str(),
            ctx: FrameCtx::from_context(ctx),
            effectors,
        };
        self.emit_if_changed(&frame);
    }

    /// Emit a frame. Best-effort, non-blocking, never panics.
    /// Returns whether the frame was written to a listener.
    pub fn emit(&mut self, frame: &TelemetryFrame) -> bool {
        let Ok(mut buf) = serde_json::to_vec(frame) else {
            return false;
        };
        buf.push(b'\n');

        // Try existing connection first, reconnect once on failure.
        for attempt in 0..2 {
            if attempt == 1 || self.stream.is_none() {
                self.stream = connect_nonblocking(&self.sock_path);
                if self.stream.is_none() {
                    return false;
                }
            }
            if let Some(ref mut s) = self.stream {
                match s.write_all(&buf) {
                    Ok(()) => return true,
                    Err(_) => {
                        self.stream = None;
                        // Allow the same fingerprint to retry after reconnect.
                        self.last_fp = None;
                    }
                }
            }
        }
        false
    }

    fn emit_if_changed(&mut self, frame: &TelemetryFrame) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        frame.kind.hash(&mut h);
        frame.from.hash(&mut h);
        frame.event.hash(&mut h);
        frame.to.hash(&mut h);
        frame.screen.hash(&mut h);
        for e in &frame.effectors {
            e.hash(&mut h);
        }
        let fp = {
            let mut combined = frame.ctx.fingerprint();
            combined ^= h.finish();
            combined
        };
        if self.last_fp == Some(fp) {
            return;
        }
        // Only burn the fingerprint after a successful write so a later GUI
        // bind still receives the current snapshot.
        if self.emit(frame) {
            self.last_fp = Some(fp);
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn connect_nonblocking(path: &PathBuf) -> Option<UnixStream> {
    let stream = UnixStream::connect(path).ok()?;
    stream.set_nonblocking(true).ok()?;
    debug!("telemetry: connected to {}", path.display());
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_serialization_roundtrip() {
        let frame = TelemetryFrame {
            ts: 1719100000000,
            kind: "transition",
            from: "LID_OPEN",
            event: "LidClose",
            to: "COUNTDOWN",
            screen: "SCREEN_ACTIVE",
            ctx: FrameCtx {
                lid_closed: true,
                ext_mon_count: 0,
                inhibitor: false,
                locked: false,
                on_ac: true,
                low_battery: false,
                power_base: "ac".into(),
                desired_profile: "balanced".into(),
                applied_profile: "balanced".into(),
                active_profile: "ultrawide".into(),
            },
            effectors: vec!["start_grace_timer"],
        };

        let json = serde_json::to_string(&frame).expect("serialize");
        assert!(json.contains("\"kind\":\"transition\""));
        assert!(json.contains("\"from\":\"LID_OPEN\""));
        assert!(json.contains("\"event\":\"LidClose\""));
        assert!(json.contains("\"to\":\"COUNTDOWN\""));
        assert!(json.contains("\"screen\":\"SCREEN_ACTIVE\""));
        assert!(json.contains("\"lid_closed\":true"));
        assert!(json.contains("\"ext_mon_count\":0"));
        assert!(json.contains("\"power_base\":\"ac\""));
        assert!(json.contains("\"active_profile\":\"ultrawide\""));
        assert!(json.contains("\"effectors\":[\"start_grace_timer\"]"));

        let val: serde_json::Value = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(val["ts"], 1719100000000u64);
        assert_eq!(val["kind"], "transition");
    }

    #[test]
    fn fingerprint_ignores_identical_ctx() {
        let a = FrameCtx {
            lid_closed: false,
            ext_mon_count: 2,
            inhibitor: false,
            locked: false,
            on_ac: true,
            low_battery: false,
            power_base: "docked-ac".into(),
            desired_profile: "balanced".into(),
            applied_profile: "balanced".into(),
            active_profile: "dual".into(),
        };
        let b = a.clone();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }
}
