//! sysfs/procfs readers. All best-effort: missing files and read errors
//! resolve to defaults, mirroring v1's tolerance of partial hardware.

use std::fs;
use std::path::Path;

use crate::paths;
use crate::pure::gpu::{GpuCard, GpuSnapshot};

/// First whitespace-separated word of a file, or None when unreadable/empty.
pub fn read_first_word(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.split_whitespace().next().map(str::to_string)
}

/// Integer file content with v1's base-0 semantics (a leading 0x reads as
/// hex); `default` on any failure.
pub fn read_int(path: &Path, default: u64) -> u64 {
    let Ok(text) = fs::read_to_string(path) else {
        return default;
    };
    let s = text.trim();
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse()
    };
    parsed.unwrap_or(default)
}

/// (external, edp) connected-connector counts for cardN.
fn card_connectors(card: &str) -> (u32, u32) {
    let (mut external, mut edp) = (0, 0);
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return (0, 0);
    };
    let prefix = format!("{card}-");
    for entry in entries.flatten() {
        let conn = entry.file_name().to_string_lossy().into_owned();
        if !conn.starts_with(&prefix) || conn.contains("-Writeback-") {
            continue;
        }
        let status = entry.path().join("status");
        match fs::read_to_string(&status) {
            Ok(s) if s.trim() == "connected" => {}
            _ => continue,
        }
        if conn.contains("eDP") {
            edp += 1;
        } else {
            external += 1;
        }
    }
    (external, edp)
}

/// Lid state without logind (D-Bus is unavailable at `gpu select` time).
pub fn lid_closed_sysfs() -> bool {
    let Ok(entries) = fs::read_dir(paths::LID_STATE_GLOB_DIR) else {
        return false;
    };
    for entry in entries.flatten() {
        if let Ok(text) = fs::read_to_string(entry.path().join("state")) {
            return text.contains("closed");
        }
    }
    false
}

/// Enumerate GPU candidates from /dev/dri/by-path. A candidate must be a
/// PCI display-class device (class 0x03*) with no usb segment in its
/// by-path name — this excludes DisplayLink/evdi and platform devices,
/// which are never listed in AQ_DRM_DEVICES (untested scanout path). If
/// such a non-candidate has a connected output, the snapshot flags it so
/// selection can bail to today's open-all-GPUs behavior instead of killing
/// that output.
pub fn gpu_snapshot() -> GpuSnapshot {
    let mut cards: Vec<GpuCard> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut non_pci_display = false;

    let mut entries: Vec<_> = fs::read_dir(paths::DRI_BY_PATH)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with("-card"))
                .collect()
        })
        .unwrap_or_default();
    entries.sort();

    for link in entries {
        let card = fs::canonicalize(&link)
            .ok()
            .and_then(|real| real.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if !card.starts_with("card") || seen.contains(&card) {
            continue;
        }
        seen.push(card.clone());
        let (external, edp) = card_connectors(&card);
        let dev = Path::new("/sys/class/drm").join(&card).join("device");
        let pci_class = fs::read_to_string(dev.join("class"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let name = link
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_candidate = name.starts_with("pci-")
            && !name.contains("-usb-")
            && !name.contains("-usbv2-")
            && pci_class.starts_with("0x03");
        if !is_candidate {
            if external > 0 || edp > 0 {
                non_pci_display = true;
            }
            continue;
        }
        cards.push(GpuCard {
            path: link.to_string_lossy().into_owned(),
            card,
            boot_vga: read_int(&dev.join("boot_vga"), 0) as u32,
            vram: read_int(&dev.join("mem_info_vram_total"), 0),
            external,
            edp,
        });
    }

    GpuSnapshot {
        cards,
        non_pci_display,
        lid_closed: lid_closed_sysfs(),
    }
}
