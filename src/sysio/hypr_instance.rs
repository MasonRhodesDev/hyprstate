use std::fs;
use std::path::PathBuf;

pub fn hyprland_pid() -> Option<u32> {
    let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let lock = PathBuf::from(runtime)
        .join("hypr")
        .join(sig)
        .join("hyprland.lock");
    let pid: u32 = fs::read_to_string(lock)
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()?;
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    (comm.trim() == "Hyprland").then_some(pid)
}

pub fn hyprland_aq_devices() -> Option<Vec<String>> {
    let pid = hyprland_pid()?;
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    for chunk in environ.split(|&b| b == 0) {
        if let Some(val) = chunk.strip_prefix(b"AQ_DRM_DEVICES=") {
            let val = String::from_utf8_lossy(val);
            return Some(
                val.split(':')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect(),
            );
        }
    }
    Some(Vec::new())
}
