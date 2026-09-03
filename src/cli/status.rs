//! `status`: systemctl + journalctl + inhibitor + gpu + power summary.
//! Subprocesses inherit stdout (their output streams through).

use std::process::Command;

use crate::paths;

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
    let conf = crate::sysio::power_conf::load_power_conf();
    if conf.lid == crate::pure::power::LidMode::Absent {
        println!("\nlid: absent (power.conf) — no handle-lid-switch inhibitor expected");
    } else {
        println!("\nlid: present — handle-lid-switch inhibitor held by hyprstate");
    }
    if paths::suspend_request_standing() {
        println!("idle-suspend request: standing");
    }
    println!("\n=== logind inhibitors ===");
    run_inherit("systemd-inhibit", &["--list", "--no-pager"]);
    println!("\n=== gpu selection ===");
    super::gpu::run("status");
    println!("\n=== power profile ===");
    super::power::run("status", None, false);
    0
}
