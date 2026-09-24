//! smol — ship and run software with isolation by default.

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod commands;

#[derive(Parser)]
#[command(name = "smol")]
#[command(about = "Ship and run software with isolation by default")]
#[command(version)]
struct Cli {
    /// Increase log verbosity (repeatable): -v info, -vv debug, -vvv trace.
    /// Logs go to stderr. Overridden by SMOL_LOG / RUST_LOG when either is set.
    ///
    /// Note: the short `-v` is intentionally omitted at the top level because
    /// `run` and `create` already use `-v` for `--volume`; use `--verbose`.
    #[arg(long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command in an ephemeral VM (cleaned up after exit)
    Run(commands::run::RunCmd),

    /// Scaffold a new project: app code, Smolfile, and a README (templates: flask, node)
    New(commands::new::NewCmd),

    /// Manage machines: create, start, stop, rm, ls, status, exec, shell, logs, cp, fork
    Machine(commands::machine::MachineCmd),

    /// Work with a Smolfile: init, up, down
    File(commands::file::FileCmd),

    /// Build + publish portable .smolmachine artifacts (create, push, pull, inspect)
    #[command(subcommand)]
    Pack(commands::pack::PackCmd),

    /// Work with registries: ls, catalog, tags, login, logout
    Registry(commands::registry::RegistryCmd),

    /// Registry + cloud authentication (login, logout)
    Auth(commands::auth::AuthCmd),

    /// Manage machines on the smolfleet cloud (deploy, ls, rm, scale, shell)
    Cloud(commands::cloud::CloudCmd),

    /// Manage CLI configuration
    Config(commands::config::ConfigCmd),

    /// Manage framework-aware fused rollout executors and policy versions
    Rollout(commands::rollout::RolloutCmd),

    /// Run an agent (Claude Code, or any program) in its own machine as a session of
    /// turns: start, send, log, rewind, fork, pause, resume, rm
    Agent(commands::agent::AgentCmd),

    /// Internal: boot a VM subprocess (not for direct use)
    #[command(name = "_boot-vm", hide = true)]
    BootVm {
        /// Path to boot config JSON file
        config: std::path::PathBuf,
    },

    /// Internal: the shared CUDA daemon (not for direct use). The engine
    /// spawns `current_exe() _cuda-daemon <socket>` on first CUDA use, and
    /// current_exe is THIS binary — without this arm, CUDA machines silently
    /// fall back to per-VM in-process serving, which breaks fork clones (their
    /// warm GPU state lives in the shared daemon, not the golden's VMM).
    #[command(name = "_cuda-daemon", hide = true)]
    CudaDaemon {
        /// Unix socket path to listen on
        socket: std::path::PathBuf,
    },

    /// Internal: serve one isolating fork-clone connection in this dedicated
    /// worker process (own CUDA context/UVA). Spawned by the daemon.
    #[command(name = "_cuda-clone-worker", hide = true)]
    CudaCloneWorker {
        /// Inherited connection file descriptor
        fd: i32,
    },
}

/// Build the tracing `EnvFilter` for the CLI.
///
/// Precedence: an explicit `SMOL_LOG` (preferred) or `RUST_LOG` env var always
/// wins; only when neither is set do we derive a filter from the `-v` count.
///
/// The verbosity ladder targets the app crates so output stays useful instead
/// of drowning in dependency trace:
/// - 0 (no -v): `warn` (unchanged default)
/// - 1 (-v):    `smol=info,smolvm=info` (warn for everything else)
/// - 2 (-vv):   `smol=debug,smolvm=debug`
/// - 3+ (-vvv): `smol=trace,smolvm=trace`
fn verbosity_filter(verbose: u8) -> EnvFilter {
    if let Ok(s) = std::env::var("SMOL_LOG") {
        return EnvFilter::new(s);
    }
    if std::env::var("RUST_LOG").is_ok() {
        return EnvFilter::from_default_env();
    }
    let directive = match verbose {
        0 => "warn",
        1 => "warn,smol=info,smolvm=info",
        2 => "warn,smol=debug,smolvm=debug",
        _ => "warn,smol=trace,smolvm=trace",
    };
    EnvFilter::new(directive)
}

/// Initialize the tracing subscriber. Logs are written to stderr so stdout
/// stays clean for piped command output.
fn init_logging(verbose: u8) {
    tracing_subscriber::fmt()
        .with_env_filter(verbosity_filter(verbose))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn main() {
    // tokio-tungstenite (interactive cloud exec/shell) builds its rustls config
    // from the process-default CryptoProvider; with both ring and aws-lc-rs in
    // the tree, rustls 0.23 can't auto-pick one, so install ring explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Fast-path: handle the internal _boot-vm subcommand before clap parsing
    // so that the subprocess spawned by start_via_subprocess() can boot the VM.
    // std::env::current_exe() resolves to this binary (smol), so smol must
    // handle _boot-vm or all VM launches fail immediately. This path never
    // sees the parsed `--verbose` flag, so it uses an env-only filter.
    {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(|s| s.as_str()) == Some("_boot-vm") {
            init_logging(0);
            if let Some(config_path) = args.get(2) {
                let result = boot_vm(std::path::PathBuf::from(config_path));
                if let Err(e) = result {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                std::process::exit(0);
            } else {
                eprintln!("Error: _boot-vm requires a config path argument");
                std::process::exit(1);
            }
        }
    }

    let cli = Cli::parse();

    // Initialize logging from the parsed verbosity count (env vars override).
    init_logging(cli.verbose);

    let result = match cli.command {
        Commands::Run(cmd) => cmd.run(),
        Commands::New(cmd) => cmd.run(),
        Commands::Machine(cmd) => cmd.run(),
        Commands::File(cmd) => cmd.run(),
        Commands::Pack(cmd) => cmd.run(),
        Commands::Registry(cmd) => cmd.run(),
        Commands::Auth(cmd) => cmd.run(),
        Commands::Cloud(cmd) => cmd.run(),
        Commands::Config(cmd) => cmd.run(),
        Commands::Rollout(cmd) => cmd.run(),
        Commands::Agent(cmd) => cmd.run(),
        Commands::BootVm { config } => boot_vm(config).map_err(|e| anyhow::anyhow!("{}", e)),
        #[cfg(unix)]
        Commands::CudaDaemon { socket } => {
            smolvm::cuda_daemon::run(&socket).map_err(|e| anyhow::anyhow!("cuda daemon: {e}"))
        }
        #[cfg(not(unix))]
        Commands::CudaDaemon { .. } => Err(anyhow::anyhow!("the shared CUDA daemon is unix-only")),
        #[cfg(unix)]
        Commands::CudaCloneWorker { fd } => smolvm::cuda_daemon::run_clone_worker(fd)
            .map_err(|e| anyhow::anyhow!("cuda clone worker: {e}")),
        #[cfg(not(unix))]
        Commands::CudaCloneWorker { .. } => {
            Err(anyhow::anyhow!("the CUDA clone worker is unix-only"))
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

/// Boot the VM described by `config_path` in this (subprocess) context.
///
/// Delegates to the engine's own boot path so `smol` never re-implements disk
/// opening, DNS filtering, CUDA host setup or the launch configuration — every
/// launch-time feature the engine adds is available here without a matching
/// change in this crate.
fn boot_vm(config_path: std::path::PathBuf) -> smolvm::Result<()> {
    smolvm::internal_boot::run(config_path)
}
