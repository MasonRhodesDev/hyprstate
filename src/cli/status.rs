//! `status`: systemctl + journalctl + inhibitor + gpu + power summary.
//! Subprocesses inherit stdout (their output streams through).

use std::process::Command;

fn run_inherit(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).status();
}

pub fn run() -> i32 {
    println!("=== systemctl --user status hyprstate.service ===");
    run_inherit(
        "systemctl",
        &["--user", "status", "hyprstate.service", "--no-pager"],
    );
    println!("\n=== last 20 log lines ===");
    run_inherit(
        "journalctl",
        &[
            "--user",
            "-u",
            "hyprstate.service",
            "-n",
            "20",
            "--no-pager",
        ],
    );
    println!("\n=== logind handle-lid-switch inhibitor ===");
    run_inherit("systemd-inhibit", &["--list", "--no-pager"]);
    println!("\n=== gpu selection ===");
    super::gpu::run("status");
    println!("\n=== power profile ===");
    super::power::run("status", None, false);
    0
}
