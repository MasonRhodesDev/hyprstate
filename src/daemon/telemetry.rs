//! Best-effort telemetry emitter over a Unix domain socket.
//!
//! After each FSM dispatch the daemon writes one newline-delimited JSON
//! frame to `$XDG_RUNTIME_DIR/hyprstate-telemetry.sock`. The write is
//! non-blocking and fire-and-forget: if no listener is connected or the
//! socket doesn't exist, the frame is silently dropped. This module never
//! affects FSM behavior.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::debug;

use crate::pure::fsm::{EventKind, ScreenState, State};

/// A single telemetry frame emitted after a state transition.
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

/// Snapshot of world inputs at dispatch time.
#[derive(Debug, Clone, Serialize)]
pub struct FrameCtx {
    pub lid_closed: bool,
    pub ext_mon_count: u32,
    pub inhibitor: bool,
    pub locked: bool,
    pub on_ac: bool,
}

/// Persistent emitter handle. Holds a lazy connection to the socket.
pub struct TelemetryEmitter {
    sock_path: PathBuf,
    stream: Option<UnixStream>,
}

impl TelemetryEmitter {
    pub fn new() -> Self {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
        let sock_path = PathBuf::from(runtime_dir).join("hyprstate-telemetry.sock");
        Self {
            sock_path,
            stream: None,
        }
    }

    /// Emit a frame. Best-effort, non-blocking, never panics.
    pub fn emit(&mut self, frame: &TelemetryFrame) {
        let Ok(mut buf) = serde_json::to_vec(frame) else {
            return;
        };
        buf.push(b'\n');

        // Try existing connection first, reconnect once on failure.
        for attempt in 0..2 {
            if attempt == 1 || self.stream.is_none() {
                self.stream = connect_nonblocking(&self.sock_path);
                if self.stream.is_none() {
                    return;
                }
            }
            if let Some(ref mut s) = self.stream {
                match s.write_all(&buf) {
                    Ok(()) => return,
                    Err(_) => {
                        self.stream = None;
                        // retry with reconnect
                    }
                }
            }
        }
    }
}

fn connect_nonblocking(path: &PathBuf) -> Option<UnixStream> {
    let stream = UnixStream::connect(path).ok()?;
    stream.set_nonblocking(true).ok()?;
    debug!("telemetry: connected to {}", path.display());
    Some(stream)
}

/// Build a frame from pre/post state and event info.
pub fn build_frame(
    from: State,
    to: State,
    _event: EventKind,
    label: &'static str,
    screen: ScreenState,
    ctx: FrameCtx,
    effectors: Vec<&'static str>,
) -> TelemetryFrame {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    TelemetryFrame {
        ts,
        kind: "transition",
        from: from.as_str(),
        event: label,
        to: to.as_str(),
        screen: screen.as_str(),
        ctx,
        effectors,
    }
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
        assert!(json.contains("\"effectors\":[\"start_grace_timer\"]"));

        // Verify it's valid JSON by parsing back
        let val: serde_json::Value = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(val["ts"], 1719100000000u64);
        assert_eq!(val["kind"], "transition");
    }
}
