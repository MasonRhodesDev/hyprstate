mod cli;
mod daemon;
#[allow(dead_code)]
mod dbus;
mod paths;
mod powerd;
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
    #[command(about = "Run the FSM (via systemd --user)")]
    Daemon {
        #[arg(long, hide = true)]
        shadow: bool,
    },
    #[command(about = "Root power effector (systemd system service, org.hyprstate.Power1)")]
    Powerd {
        #[arg(long, hide = true)]
        session: bool,
    },
    #[command(about = "Invoked by /usr/lib/systemd/system-sleep/ as root")]
    SleepHook {
        action: Option<String>,
        #[arg(help = "suspend|hibernate|... — passed by systemd, ignored")]
        sleep_type: Option<String>,
    },
    #[command(about = "GPU-primary selection (uwsm + drift status)")]
    Gpu {
        #[arg(value_parser = ["select", "check", "status"])]
        action: String,
    },
    #[command(about = "Power profile policy")]
    Power {
        #[arg(value_parser = ["set", "get", "cycle", "status"])]
        action: String,
        value: Option<String>,
        #[arg(long)]
        waybar: bool,
    },
    #[command(about = "Monitor profiles (save captures the live layout as a new profile)")]
    Profile {
        #[arg(value_parser = ["list", "current", "switch", "save", "migrate", "verify"])]
        action: String,
        name: Option<String>,
        #[arg(long, value_parser = ["auto", "enable", "disable"], default_value = "auto", help = "save: eDP policy directive for the captured profile")]
        edp: String,
        #[arg(long, value_parser = ["auto", "igpu", "dgpu"], default_value = "auto", help = "save: render-GPU preference directive")]
        gpu: String,
        #[arg(
            long,
            help = "save: explicit `#@ priority` (default: implicit match count)"
        )]
        priority: Option<i64>,
        #[arg(long, help = "save: overwrite an existing profile")]
        force: bool,
        #[arg(long, help = "migrate: report conversions without writing TOML files")]
        dry_run: bool,
    },
    #[command(about = "Keyboard backlight: manual level keys and the policy's status")]
    Kbd {
        #[arg(value_parser = ["status", "set", "up", "down", "auto"])]
        action: String,
        #[arg(help = "set: percent 0..100")]
        value: Option<String>,
    },
    #[command(
        about = "Idle-suspend request (driven by hypridle; feeds the daemon's Countdown machinery, never suspends directly)"
    )]
    Suspend {
        #[arg(value_parser = ["request", "cancel", "status"])]
        action: String,
    },
    #[command(about = "systemctl + journalctl + gpu + power summary")]
    Status,
}

fn init_tracing() {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let prose = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("hyprstate.trace", LevelFilter::OFF);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_filter(prose),
        )
        .with(span_lines::tracing_layer::install(&["hyprstate.trace"]))
        .init();
}

fn main() {
    let cli = Cli::parse();
    let rc = match cli.cmd {
        Cmd::Daemon { shadow } => {
            init_tracing();
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
            init_tracing();
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
        Cmd::Kbd { action, value } => cli::kbd::run(&action, value.as_deref()),
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
            dry_run,
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
                dry_run,
            };
            cli::profile::run(&action, name.as_deref(), &save)
        }
        Cmd::Suspend { action } => cli::suspend::run(&action),
        Cmd::Status => cli::status::run(),
    };
    span_lines::exit(rc);
}
