//! hyprstate: lid / monitor / profile / lock / suspend / wake state machine
//! for Hyprland. Rust port of hyprstate.py (v2) — see POWER_SPEC.md and
//! GPU_SPEC.md for the behavioral contracts shared with v1.
//!
//! Architecture (daemon):
//!   Layer 1 — effectors:      narrow, idempotent world mutations.
//!   Layer 2 — on_enter:       composes effectors; the only place effects fire.
//!   Layer 3 — pure/:          (state, inputs) -> state maps; no I/O at all.

mod cli;
mod daemon;
// dead_code: a few proxy methods exist for completeness of the pinned
// interface shapes rather than current callers.
#[allow(dead_code)]
mod dbus;
mod paths;
mod powerd;
// The pure (I/O-free) FSM + policy layer now lives in its own crate so the GUI
// can share the exact same types. Aliased to `pure` so the daemon's existing
// `crate::pure::...` paths are unchanged.
use hyprstate_fsm as pure;
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
    /// Monitor profiles (save captures the live layout as a new profile)
    Profile {
        #[arg(value_parser = ["list", "current", "switch", "save"])]
        action: String,
        name: Option<String>,
        /// save: eDP policy directive for the captured profile
        #[arg(long, value_parser = ["auto", "enable", "disable"], default_value = "auto")]
        edp: String,
        /// save: render-GPU preference directive
        #[arg(long, value_parser = ["auto", "igpu", "dgpu"], default_value = "auto")]
        gpu: String,
        /// save: explicit `#@ priority` (default: implicit match count)
        #[arg(long)]
        priority: Option<i64>,
        /// save: overwrite an existing profile
        #[arg(long)]
        force: bool,
    },
    /// systemctl + journalctl + gpu + power summary
    Status,
}

fn main() {
    let cli = Cli::parse();
    let rc = match cli.cmd {
        Cmd::Daemon { shadow } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stdout)
                .init();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            match rt.block_on(daemon::run(shadow)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("daemon failed: {e:#}");
                    1
                }
            }
        }
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
        Cmd::Profile {
            action,
            name,
            edp,
            gpu,
            priority,
            force,
        } => {
            use pure::profiles::{EdpPolicy, GpuPref};
            let save = cli::profile::SaveOpts {
                edp: match edp.as_str() {
                    "enable" => EdpPolicy::Enable,
                    "disable" => EdpPolicy::Disable,
                    _ => EdpPolicy::Auto,
                },
                gpu: match gpu.as_str() {
                    "igpu" => GpuPref::Igpu,
                    "dgpu" => GpuPref::Dgpu,
                    _ => GpuPref::Auto,
                },
                priority,
                force,
            };
            cli::profile::run(&action, name.as_deref(), &save)
        }
        Cmd::Status => cli::status::run(),
    };
    std::process::exit(rc);
}
