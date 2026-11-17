use crate::paths;

pub fn run(action: &str) -> i32 {
    let Some(path) = paths::suspend_request_file() else {
        return match action {
            "request" => {
                eprintln!("hyprstate suspend request: no XDG_RUNTIME_DIR");
                1
            }
            "status" => {
                println!("idle-suspend request: none (no runtime dir)");
                0
            }
            _ => 0,
        };
    };
    match action {
        "request" => match std::fs::write(&path, "idle\n") {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("hyprstate suspend request: {}: {e}", path.display());
                1
            }
        },
        "cancel" => match std::fs::remove_file(&path) {
            Ok(()) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => {
                eprintln!("hyprstate suspend cancel: {}: {e}", path.display());
                1
            }
        },
        "status" => {
            if path.exists() {
                println!("idle-suspend request: standing ({})", path.display());
            } else {
                println!("idle-suspend request: none");
            }
            0
        }
        other => {
            eprintln!("hyprstate suspend: unknown action {other:?}");
            2
        }
    }
}
