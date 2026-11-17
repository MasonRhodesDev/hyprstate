use std::io::Write;

use crate::paths;
use crate::pure::gpu::{GpuMode, gpu_desired, resolve_gpu_mode};
use crate::sysio::{gpu_state, hypr_instance, sysfs};

fn resolve_mode_from_files() -> (GpuMode, crate::pure::gpu::GpuModeSource) {
    let override_word = sysfs::read_first_word(&paths::gpu_override_file());
    let overlay = sysfs::read_first_word(&paths::gpu_breadcrumb_file());
    let platform = sysfs::read_first_word(paths::platform_profile_path());
    let (mode, source, warnings) = resolve_gpu_mode(
        override_word.as_deref(),
        overlay.as_deref(),
        platform.as_deref(),
    );
    for w in warnings {
        eprintln!("WARNING {}: {w}", paths::gpu_override_file().display());
    }
    (mode, source)
}

pub fn run(action: &str) -> i32 {
    if action == "status" {
        return status();
    }

    let mut snap = sysfs::gpu_snapshot();
    let (mode, source) = resolve_mode_from_files();
    let (mut devices, mut reason) = gpu_desired(&snap, mode, source);

    if devices.is_none() && reason == "bailed-transient" && action == "select" {
        std::thread::sleep(paths::GPU_SETTLE);
        snap = sysfs::gpu_snapshot();
        (devices, reason) = gpu_desired(&snap, mode, source);
    }

    if let Some(list) = &devices
        && !list.iter().all(|d| std::path::Path::new(d).exists())
    {
        (devices, reason) = (None, "validation-failed".into());
    }

    if action == "select" {
        gpu_state::write_gpu_state(mode.as_str(), &reason, devices.as_deref(), &snap);
    }

    if let Some(list) = devices {
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{}", list.join(":"));
        let _ = stdout.flush();
    }
    0
}

fn status() -> i32 {
    match gpu_state::read_gpu_state() {
        Some(state) => {
            println!("intent : mode={} reason={}", state.mode, state.reason);
            let devices = if state.devices.is_empty() {
                "(none)".to_string()
            } else {
                state.devices.join(":")
            };
            println!("         devices={devices}");
        }
        None => println!("intent : (no state file)"),
    }

    let actual = hypr_instance::hyprland_aq_devices();
    match &actual {
        None => println!("actual : (no Hyprland session found)"),
        Some(list) if list.is_empty() => {
            println!("actual : (compositor defaults — AQ_DRM_DEVICES unset)")
        }
        Some(list) => println!("actual : {}", list.join(":")),
    }

    let (mode, source) = resolve_mode_from_files();
    let (desired, reason) = gpu_desired(&sysfs::gpu_snapshot(), mode, source);
    match &desired {
        None => println!("desired: (unmanaged — {reason})"),
        Some(list) => println!(
            "desired: {}  (mode={}/{}, {reason})",
            list.join(":"),
            mode.as_str(),
            source.as_str()
        ),
    }

    let primary_card = actual
        .as_ref()
        .and_then(|l| l.first())
        .and_then(|d| d.rsplit('/').next())
        .map(str::to_string);

    if let (Some(actual), Some(desired)) = (&actual, &desired) {
        if desired == actual {
            println!("sync   : in sync");
        } else {
            println!("sync   : MISMATCH — relog to apply");
        }
    }

    let outputs = sysfs::connected_outputs();
    if !outputs.is_empty() {
        println!("outputs:");
        for (connector, card) in outputs {
            let tag = match &primary_card {
                Some(p) if *p == card => "render-local (no copy)".to_string(),
                Some(p) => format!("COPY PATH {card}->{p}"),
                None => "render GPU unknown".to_string(),
            };
            println!("         {connector:<8} {card}  {tag}");
        }
    }
    0
}
