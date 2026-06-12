//! powerd's sysfs knob matrix (see POWER_SPEC.md). Mechanism only, no
//! policy: three profile names map onto a hardcoded whitelist of sysfs
//! writes. Every row is read-before-write idempotent; row failures land as
//! result strings, never as a failed ApplyProfile.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::paths;
use crate::pure::gpu::{integrated_card, pci_key};
use crate::pure::power::{PowerProfile, platform_profile_chain};
use crate::sysio::sysfs::gpu_snapshot;

fn governor(p: PowerProfile) -> &'static str {
    match p {
        PowerProfile::PowerSaver | PowerProfile::Balanced => "powersave",
        PowerProfile::Performance => "performance",
    }
}

/// EPP value; None for performance (implied by the performance governor,
/// and writing it under that governor EBUSYs anyway).
fn epp(p: PowerProfile) -> Option<&'static str> {
    match p {
        PowerProfile::PowerSaver => Some("power"),
        PowerProfile::Balanced => Some("balance_performance"),
        PowerProfile::Performance => None,
    }
}

fn boost(p: PowerProfile) -> &'static str {
    match p {
        PowerProfile::PowerSaver => "0",
        PowerProfile::Balanced | PowerProfile::Performance => "1",
    }
}

fn dgpu_dpm(p: PowerProfile) -> &'static str {
    match p {
        PowerProfile::PowerSaver => "low",
        PowerProfile::Balanced | PowerProfile::Performance => "auto",
    }
}

fn aspm(p: PowerProfile) -> &'static str {
    match p {
        PowerProfile::PowerSaver => "powersupersave",
        PowerProfile::Balanced | PowerProfile::Performance => "default",
    }
}

/// Idempotent single-knob write -> result enum string. EBUSY/EPERM/EACCES
/// are 'skipped-unsupported' (EPP locked by performance governor,
/// BIOS-locked turbo, BIOS-disabled ASPM) — expected hardware conditions,
/// not errors.
fn knob_write(path: &Path, value: &str) -> String {
    match fs::read_to_string(path) {
        Ok(cur) if cur.trim() == value => return "unchanged".into(),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return "skipped-missing".into(),
        Err(e) => return format!("error:{e}"),
    }
    match fs::write(path, value) {
        Ok(()) => "written".into(),
        Err(e) => match e.raw_os_error() {
            Some(libc::EBUSY) | Some(libc::EPERM) | Some(libc::EACCES) => {
                "skipped-unsupported".into()
            }
            _ => format!("error:{e}"),
        },
    }
}

/// Collapse per-CPU-policy statuses into one result entry.
fn merge_row(statuses: &[String]) -> String {
    if statuses.is_empty() {
        return "skipped-missing".into();
    }
    if let Some(err) = statuses.iter().find(|s| s.starts_with("error")) {
        return err.clone();
    }
    if statuses.iter().any(|s| s == "written") {
        return "written".into();
    }
    if statuses.iter().all(|s| s == "unchanged") {
        return "unchanged".into();
    }
    statuses[0].clone()
}

fn cpufreq_policies() -> Vec<PathBuf> {
    let mut policies: Vec<PathBuf> = fs::read_dir(paths::CPUFREQ_DIR)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_prefix("policy"))
                        .is_some_and(|rest| {
                            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    policies.sort();
    policies
}

fn cpu_rows(profile: PowerProfile, res: &mut HashMap<String, String>) {
    let policies = cpufreq_policies();
    let Some(p0) = policies.first() else {
        res.insert("cpu".into(), "skipped-missing".into());
        return;
    };
    let driver = fs::read_to_string(p0.join("scaling_driver"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let epp_capable = p0.join("energy_performance_preference").exists()
        || matches!(driver.as_str(), "amd-pstate-epp" | "intel_pstate");
    if !epp_capable {
        // On acpi-cpufreq "powersave" PINS MIN FREQUENCY (our balanced
        // profile would crawl); on schedutil kernels our values don't exist.
        // Only EPP drivers interpret powersave/performance the way this
        // matrix means.
        res.insert("scaling_governor".into(), "skipped-unsupported".into());
        res.insert(
            "energy_performance_preference".into(),
            "skipped-unsupported".into(),
        );
        return;
    }
    let gov = governor(profile);
    let gov_statuses: Vec<String> = policies
        .iter()
        .map(|pol| {
            let avail =
                fs::read_to_string(pol.join("scaling_available_governors")).unwrap_or_default();
            if avail.split_whitespace().any(|g| g == gov) {
                knob_write(&pol.join("scaling_governor"), gov)
            } else {
                "skipped-unsupported".into()
            }
        })
        .collect();
    res.insert("scaling_governor".into(), merge_row(&gov_statuses));
    if let Some(epp_val) = epp(profile) {
        // Written AFTER governor; EBUSY tolerated.
        let epp_statuses: Vec<String> = policies
            .iter()
            .map(|pol| knob_write(&pol.join("energy_performance_preference"), epp_val))
            .collect();
        res.insert(
            "energy_performance_preference".into(),
            merge_row(&epp_statuses),
        );
    }
}

fn boost_row(profile: PowerProfile, res: &mut HashMap<String, String>) {
    let boost_path = Path::new(paths::CPUFREQ_DIR).join("boost");
    let no_turbo = Path::new(paths::INTEL_NO_TURBO_PATH);
    if boost_path.exists() {
        res.insert("boost".into(), knob_write(&boost_path, boost(profile)));
    } else if no_turbo.exists() {
        // Inverted semantics: no_turbo=1 disables boost.
        let val = if profile == PowerProfile::PowerSaver {
            "1"
        } else {
            "0"
        };
        res.insert("no_turbo".into(), knob_write(no_turbo, val));
    } else {
        res.insert("boost".into(), "skipped-missing".into());
    }
}

fn gpu_rows(profile: PowerProfile, res: &mut HashMap<String, String>) {
    let snap = gpu_snapshot();
    if snap.cards.is_empty() {
        res.insert("gpu".into(), "skipped-missing".into());
        return;
    }
    let integrated = if snap.cards.len() >= 2 {
        integrated_card(&snap.cards)
    } else {
        None
    };
    let Some(integrated) = integrated else {
        // Single-card desktops must not have their only (discrete) GPU
        // misclassified and clamped; classification needs >= 2 cards.
        res.insert("gpu".into(), "skipped-ambiguous".into());
        return;
    };
    for (i, c) in snap.cards.iter().enumerate() {
        let label = format!("dpm:{}", pci_key(&c.path));
        let dev = Path::new("/sys/class/drm").join(&c.card).join("device");
        let knob = dev.join("power_dpm_force_performance_level");
        if i == integrated {
            res.insert(label, knob_write(&knob, "auto"));
            continue;
        }
        // runtime_status FIRST — opening the dpm knob on a runtime-suspended
        // card wakes it, destroying the GPU-omission power win.
        match fs::read_to_string(dev.join("power/runtime_status")) {
            Ok(s) if s.trim() == "suspended" => {
                res.insert(label, "skipped-suspended".into());
            }
            Ok(_) => {
                res.insert(label, knob_write(&knob, dgpu_dpm(profile)));
            }
            Err(_) => {
                res.insert(label, "skipped-missing".into());
            }
        }
    }
}

/// The currently-selected ASPM policy is the bracket-annotated word.
fn aspm_current(opts: &str) -> Option<String> {
    opts.split_whitespace()
        .find(|o| o.starts_with('['))
        .map(|o| o.trim_matches(['[', ']']).to_string())
}

fn aspm_row(profile: PowerProfile, res: &mut HashMap<String, String>, writable: bool) {
    let path = Path::new(paths::ASPM_POLICY_PATH);
    if !path.exists() {
        res.insert("pcie_aspm".into(), "skipped-missing".into());
        return;
    }
    if !writable {
        res.insert("pcie_aspm".into(), "skipped-unsupported".into()); // BIOS-disabled ASPM
        return;
    }
    let opts = match fs::read_to_string(path) {
        Ok(o) => o,
        Err(e) => {
            res.insert("pcie_aspm".into(), format!("error:{e}"));
            return;
        }
    };
    let target = aspm(profile);
    let valid = opts
        .split_whitespace()
        .any(|o| o.trim_matches(['[', ']']) == target);
    let status = if !valid {
        "skipped-unsupported".into()
    } else if aspm_current(&opts).as_deref() == Some(target) {
        "unchanged".into()
    } else {
        knob_write(path, target)
    };
    res.insert("pcie_aspm".into(), status);
}

fn platform_row(profile: PowerProfile, res: &mut HashMap<String, String>) {
    if !paths::platform_profile_path().exists() {
        res.insert("platform_profile".into(), "skipped-missing".into());
        return;
    }
    let choices = fs::read_to_string(paths::PLATFORM_PROFILE_CHOICES_PATH).unwrap_or_default();
    let choices: Vec<&str> = choices.split_whitespace().collect();
    let target = platform_profile_chain(profile)
        .iter()
        .find(|v| choices.contains(v));
    let status = match target {
        Some(v) => knob_write(paths::platform_profile_path(), v),
        None => "skipped-unsupported".into(),
    };
    res.insert("platform_profile".into(), status);
}

/// Apply the whitelist for `profile`. Rows are independent; a row that hits
/// unexpected I/O conditions reports `error:<msg>` in its slot.
pub fn powerd_apply(profile: PowerProfile, aspm_writable: bool) -> HashMap<String, String> {
    let mut res = HashMap::new();
    platform_row(profile, &mut res);
    cpu_rows(profile, &mut res);
    boost_row(profile, &mut res);
    gpu_rows(profile, &mut res);
    aspm_row(profile, &mut res, aspm_writable);
    res
}

/// Persisted active profile; invalid/missing -> balanced + warning.
pub fn persisted_profile() -> PowerProfile {
    if let Ok(text) = fs::read_to_string(paths::POWERD_STATE_FILE)
        && let Some(word) = text.split_whitespace().next()
    {
        if let Some(p) = PowerProfile::from_str(word) {
            return p;
        }
        warn!("persisted profile {word:?} invalid — using balanced");
    }
    PowerProfile::Balanced
}

pub fn persist_profile(profile: PowerProfile) {
    let state = Path::new(paths::POWERD_STATE_FILE);
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = state.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = state.with_extension("tmp");
        fs::write(&tmp, format!("{}\n", profile.as_str()))?;
        fs::rename(&tmp, state)
    })();
    if let Err(e) = result {
        warn!("persist failed: {e}");
    }
}

/// Read-only live values for GetKnobs / status.
pub fn knob_snapshot() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let cpufreq = Path::new(paths::CPUFREQ_DIR);
    let labeled: [(&str, PathBuf); 6] = [
        (
            "platform_profile",
            paths::platform_profile_path().to_path_buf(),
        ),
        ("scaling_governor", cpufreq.join("policy0/scaling_governor")),
        (
            "energy_performance_preference",
            cpufreq.join("policy0/energy_performance_preference"),
        ),
        ("boost", cpufreq.join("boost")),
        ("no_turbo", PathBuf::from(paths::INTEL_NO_TURBO_PATH)),
        ("pcie_aspm", PathBuf::from(paths::ASPM_POLICY_PATH)),
    ];
    for (label, path) in labeled {
        if let Ok(text) = fs::read_to_string(&path) {
            out.insert(label.to_string(), text.trim().to_string());
        }
    }
    for c in gpu_snapshot().cards {
        let dev = Path::new("/sys/class/drm").join(&c.card).join("device");
        let label = format!("dpm:{}", pci_key(&c.path));
        match fs::read_to_string(dev.join("power/runtime_status")) {
            Ok(s) if s.trim() == "suspended" => {
                out.insert(label, "(runtime-suspended)".into());
            }
            Ok(_) => {
                if let Ok(v) = fs::read_to_string(dev.join("power_dpm_force_performance_level")) {
                    out.insert(label, v.trim().to_string());
                }
            }
            Err(_) => {}
        }
    }
    out
}

/// One same-value rewrite probe at startup; EPERM means BIOS-disabled ASPM
/// and the row is skipped-unsupported forever.
pub fn aspm_writability_probe() -> bool {
    let path = Path::new(paths::ASPM_POLICY_PATH);
    let Ok(opts) = fs::read_to_string(path) else {
        return false;
    };
    let Some(cur) = aspm_current(&opts) else {
        return false;
    };
    match fs::write(path, &cur) {
        Ok(()) => true,
        Err(e) => {
            tracing::info!("ASPM not writable ({e}) — row disabled");
            false
        }
    }
}
