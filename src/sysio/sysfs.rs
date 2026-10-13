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

/// Every connected, non-writeback output as `(connector, card)` — e.g.
/// `("DP-1", "card1")`. Drives `gpu status`'s per-output view: which GPU each
/// display hangs off, hence whether it's on the cross-GPU copy path.
pub fn connected_outputs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `cardN-<connector>`; the bare `cardN` device and `renderD*` have no
        // '-' and are skipped, as is `*-Writeback-*`.
        let Some((card, connector)) = name.split_once('-') else {
            continue;
        };
        if !card.starts_with("card") || name.contains("-Writeback-") {
            continue;
        }
        if matches!(fs::read_to_string(entry.path().join("status")), Ok(s) if s.trim() == "connected")
        {
            out.push((connector.to_string(), card.to_string()));
        }
    }
    out.sort();
    out
}

/// AC state from /sys/class/power_supply, the UPower-down fallback. `Some`
/// for the first supply whose `type` is `Mains`; `None` when there is none
/// (desktop, VM, or no adapter driver). Matched on `type`, not on the
/// name: Framework's adapter is `ACAD`, the Asahi macsmc driver's is
/// `macsmc-ac`, and a name filter that fits one misses the other.
pub fn on_ac_sysfs() -> Option<bool> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    let mut supplies: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let kind = fs::read_to_string(entry.path().join("type")).ok()?;
            let online = fs::read_to_string(entry.path().join("online")).unwrap_or_default();
            Some((kind, online))
        })
        .collect();
    supplies.sort();
    on_ac_from_supplies(&supplies)
}

/// Pure half of `on_ac_sysfs`: `(type, online)` file contents per supply.
pub fn on_ac_from_supplies(supplies: &[(String, String)]) -> Option<bool> {
    supplies
        .iter()
        .find(|(kind, _)| kind.trim() == "Mains")
        .map(|(_, online)| online.trim() == "1")
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

#[cfg(test)]
mod tests {
    use super::on_ac_from_supplies;

    fn s(kind: &str, online: &str) -> (String, String) {
        (format!("{kind}\n"), format!("{online}\n"))
    }

    /// Framework 16: the adapter is `ACAD`; a USB-C source may sit beside it.
    #[test]
    fn framework_adapter_is_matched_by_type() {
        let supplies = [s("Battery", ""), s("Mains", "1"), s("USB", "0")];
        assert_eq!(on_ac_from_supplies(&supplies), Some(true));
    }

    /// Asahi: `macsmc-ac` starts with no `A`; a name filter never saw it.
    #[test]
    fn asahi_adapter_is_matched_by_type_not_name() {
        let supplies = [s("Mains", "0"), s("Battery", "")];
        assert_eq!(on_ac_from_supplies(&supplies), Some(false));
    }

    /// A desktop has no Mains supply: unknown, not "on battery".
    #[test]
    fn no_mains_supply_is_unknown() {
        assert_eq!(on_ac_from_supplies(&[s("Battery", "")]), None);
        assert_eq!(on_ac_from_supplies(&[]), None);
    }
}
