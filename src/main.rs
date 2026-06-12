//! hyprstate: lid / monitor / profile / lock / suspend / wake state machine
//! for Hyprland. Rust port of hyprstate.py (v2) — see POWER_SPEC.md and
//! GPU_SPEC.md for the behavioral contracts shared with v1.
//!
//! Architecture (daemon):
//!   Layer 1 — effectors:      narrow, idempotent world mutations.
//!   Layer 2 — on_enter:       composes effectors; the only place effects fire.
//!   Layer 3 — pure/:          (state, inputs) -> state maps; no I/O at all.

mod cli;
// dead_code: parts of the dbus/pure/sysio layers are consumed by milestones
// that haven't landed yet (daemon, remaining CLIs).
#[allow(dead_code)]
mod dbus;
mod paths;
mod powerd;
#[allow(dead_code)]
mod pure;
mod sleep_hook;
#[allow(dead_code)]
mod sysio;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hyprstate", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the FSM (via systemd --user)
    Daemon {
        /// Log decisions only: no effectors fire, no lid inhibitor is taken.
        /// Safe to run alongside the live (Python) daemon for parity diffing.
        #[arg(long, hide = true)]
        shadow: bool,
    },
    /// Root power effector (systemd system service, org.hyprstate.Power1)
    Powerd {
        /// Dev/testing: serve on the session bus instead of the system bus.
        /// Sysfs writes degrade to skipped-unsupported when unprivileged.
        #[arg(long, hide = true)]
        session: bool,
    },
    /// Invoked by /usr/lib/systemd/system-sleep/ as root
    SleepHook {
        action: Option<String>,
        /// suspend|hibernate|... — passed by systemd, ignored
        sleep_type: Option<String>,
    },
    /// GPU-primary selection (uwsm + drift status)
    Gpu {
        #[arg(value_parser = ["select", "check", "status"])]
        action: String,
    },
    /// Power profile policy
    Power {
        #[arg(value_parser = ["set", "get", "cycle", "status"])]
        action: String,
        value: Option<String>,
        #[arg(long)]
        waybar: bool,
    },
    /// Monitor profiles
    Profile {
        #[arg(value_parser = ["list", "current", "switch"])]
        action: String,
        name: Option<String>,
    },
    /// systemctl + journalctl + gpu + power summary
    Status,
}

fn main() {
    let cli = Cli::parse();
    let rc = match cli.cmd {
        Cmd::Daemon { .. } => todo("daemon"),
        Cmd::Powerd { session } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stdout)
                .init();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            match rt.block_on(powerd::run(session)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("powerd failed: {e:#}");
                    1
                }
            }
        }
        Cmd::SleepHook {
            action,
            sleep_type: _,
        } => match action {
            Some(a) => sleep_hook::run(&a),
            None => {
                eprintln!("sleep-hook requires pre|post");
                1
            }
        },
        Cmd::Gpu { action } => cli::gpu::run(&action),
        Cmd::Power {
            action,
            value,
            waybar,
        } => cli::power::run(&action, value.as_deref(), waybar),
        Cmd::Profile { action, name } => cli::profile::run(&action, name.as_deref()),
        Cmd::Status => cli::status::run(),
    };
    std::process::exit(rc);
}

fn todo(cmd: &str) -> i32 {
    eprintln!("hyprstate v2: `{cmd}` is not ported yet — use hyprstate.py (v1)");
    2
}
