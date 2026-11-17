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
    Daemon {
        #[arg(long, hide = true)]
        shadow: bool,
    },
    Powerd {
        #[arg(long, hide = true)]
        session: bool,
    },
    SleepHook {
        action: Option<String>,
        sleep_type: Option<String>,
    },
    Gpu {
        #[arg(value_parser = ["select", "check", "status"])]
        action: String,
    },
    Power {
        #[arg(value_parser = ["set", "get", "cycle", "status"])]
        action: String,
        value: Option<String>,
        #[arg(long)]
        waybar: bool,
    },
    Profile {
        #[arg(value_parser = ["list", "current", "switch", "save", "migrate", "verify"])]
        action: String,
        name: Option<String>,
        #[arg(long, value_parser = ["auto", "enable", "disable"], default_value = "auto")]
        edp: String,
        #[arg(long, value_parser = ["auto", "igpu", "dgpu"], default_value = "auto")]
        gpu: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
    Kbd {
        #[arg(value_parser = ["status", "set", "up", "down", "auto"])]
        action: String,
        value: Option<String>,
    },
    Suspend {
        #[arg(value_parser = ["request", "cancel", "status"])]
        action: String,
    },
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
