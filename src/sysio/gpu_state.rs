//! The gpu-select state file: $XDG_RUNTIME_DIR/hypr-gpu-primary.json,
//! schema v1 (see GPU_SPEC.md). Written by `gpu select` (intent record),
//! read by the daemon's drift check and `gpu status`. Ground truth for
//! *actual* is Hyprland's environ, never this file.

use std::collections::BTreeMap;
use std::fs;

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::pure::gpu::{GpuSnapshot, devnode, integrated_card};

#[derive(Debug, Serialize, Deserialize)]
pub struct GpuStateFile {
    pub version: u32,
    pub mode: String,
    pub reason: String,
    pub primary: Option<String>,
    pub devices: Vec<String>,
    pub omitted: Vec<String>,
    pub snapshot: BTreeMap<String, CardRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CardRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub boot_vga: u32,
    pub vram: u64,
    pub external: u32,
    pub edp: u32,
}

fn snapshot_key(path: &str) -> String {
    crate::pure::gpu::pci_key(path).to_string()
}

/// Best-effort atomic intent record. Never panics, never touches stdout
/// (the caller's stdout is consumed raw by uwsm).
pub fn write_gpu_state(mode: &str, reason: &str, devices: Option<&[String]>, snap: &GpuSnapshot) {
    let integrated = if snap.cards.len() >= 2 {
        integrated_card(&snap.cards)
    } else {
        None
    };
    let device_list: Vec<String> = devices.map(<[String]>::to_vec).unwrap_or_default();
    let payload = GpuStateFile {
        version: 1,
        mode: mode.to_string(),
        reason: reason.to_string(),
        primary: device_list.first().cloned(),
        omitted: snap
            .cards
            .iter()
            .map(devnode)
            .filter(|d| devices.is_none() || !device_list.contains(d))
            .collect(),
        devices: device_list,
        snapshot: snap
            .cards
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    snapshot_key(&c.path),
                    CardRecord {
                        kind: if Some(i) == integrated {
                            "integrated"
                        } else {
                            "discrete"
                        }
                        .to_string(),
                        boot_vga: c.boot_vga,
                        vram: c.vram,
                        external: c.external,
                        edp: c.edp,
                    },
                )
            })
            .collect(),
    };

    let state = paths::gpu_state_path();
    let tmp = state.with_extension("tmp");
    let result = serde_json::to_string_pretty(&payload)
        .map_err(|e| e.to_string())
        .and_then(|json| fs::write(&tmp, json).map_err(|e| e.to_string()))
        .and_then(|()| fs::rename(&tmp, &state).map_err(|e| e.to_string()));
    if let Err(e) = result {
        eprintln!("WARNING gpu state write failed: {e}");
    }
}

pub fn read_gpu_state() -> Option<GpuStateFile> {
    let text = fs::read_to_string(paths::gpu_state_path()).ok()?;
    serde_json::from_str(&text).ok()
}
