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

    /// Execute a command in a running machine
    Exec(commands::exec::ExecCmd),

    /// Open an interactive shell in a machine
    #[command(visible_alias = "sh")]
    Shell {
        /// Machine name (default: "default")
        #[arg(short = 'n', long, value_name = "NAME")]
        name: Option<String>,
    },

    /// Create a persistent machine
    Create(commands::create::CreateCmd),

    /// Start a machine
    Start(commands::start::StartCmd),

    /// Fork a running, forkable machine into a new clone (CoW RAM + disks)
    Fork(commands::fork::ForkCmd),

    /// Stop a machine
    Stop(commands::stop::StopCmd),

    /// Delete a machine
    #[command(visible_alias = "delete")]
    Rm(commands::rm::RmCmd),

    /// List machines
    #[command(visible_alias = "list")]
    Ls(commands::ls::LsCmd),

    /// Copy files between host and machine
    Cp(commands::cp::CpCmd),

    /// Stream machine logs
    Logs(commands::logs::LogsCmd),

    /// Show machine details
    Status(commands::status::StatusCmd),

    /// Per-machine maintenance: images, prune, update, monitor, data-dir
    Machine(commands::machine::MachineCmd),

    /// Create a Smolfile in the current directory
    Init(commands::init::InitCmd),

    /// Start a machine from a Smolfile
    Up(commands::up::UpCmd),

    /// Stop the machine started by `smol up`
    Down(commands::down::DownCmd),

    /// Build + publish portable .smolmachine artifacts (create, push, pull, inspect)
    #[command(subcommand)]
    Pack(commands::pack::PackCmd),

    /// Registry + cloud authentication (login, logout)
    Auth(commands::auth::AuthCmd),

    /// Manage machines on the smolfleet cloud (deploy, ls, rm, scale, shell)
    Cloud(commands::cloud::CloudCmd),

    /// Manage CLI configuration
    Config(commands::config::ConfigCmd),

    /// Internal: boot a VM subprocess (not for direct use)
    #[command(name = "_boot-vm", hide = true)]
    BootVm {
        /// Path to boot config JSON file
        config: std::path::PathBuf,
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
        Commands::Exec(cmd) => cmd.run(),
        Commands::Shell { name } => commands::exec::ExecCmd {
            name,
            command: vec!["/bin/sh".to_string()],
            interactive: true,
            tty: true,
            stream: false,
            env: vec![],
            workdir: None,
            secret_env: vec![],
            secret_file: vec![],
            timeout: None,
            cloud: false,
        }
        .run(),
        Commands::Create(cmd) => cmd.run(),
        Commands::Start(cmd) => cmd.run(),
        Commands::Fork(cmd) => cmd.run(),
        Commands::Stop(cmd) => cmd.run(),
        Commands::Rm(cmd) => cmd.run(),
        Commands::Ls(cmd) => cmd.run(),
        Commands::Cp(cmd) => cmd.run(),
        Commands::Logs(cmd) => cmd.run(),
        Commands::Status(cmd) => cmd.run(),
        Commands::Machine(cmd) => cmd.run(),
        Commands::Init(cmd) => cmd.run(),
        Commands::Up(cmd) => cmd.run(),
        Commands::Down(cmd) => cmd.run(),
        Commands::Pack(cmd) => cmd.run(),
        Commands::Auth(cmd) => cmd.run(),
        Commands::Cloud(cmd) => cmd.run(),
        Commands::Config(cmd) => cmd.run(),
        Commands::BootVm { config } => boot_vm(config).map_err(|e| anyhow::anyhow!("{}", e)),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

/// Run the internal VM boot subprocess.
///
/// Identical to smolvm's `src/cli/internal_boot.rs::run()`.
/// Must be kept in sync when internal_boot.rs changes.
fn boot_vm(config_path: std::path::PathBuf) -> smolvm::Result<()> {
    use smolvm::agent::boot_config::BootConfig;
    use smolvm::agent::{launch_agent_vm, LaunchConfig, VmDisks};

    // Become a session leader (detach from parent's terminal session)
    unsafe {
        libc::setsid();
    }

    // Read boot config
    let config_data = std::fs::read(&config_path)
        .map_err(|e| smolvm::Error::agent("read boot config", e.to_string()))?;
    let config: BootConfig = serde_json::from_slice(&config_data)
        .map_err(|e| smolvm::Error::agent("parse boot config", e.to_string()))?;

    // Clean up the config file — it's no longer needed
    let _ = std::fs::remove_file(&config_path);

    // Redirect stdio
    if let Err(e) = smolvm::process::detach_stdio_to_stderr_file(&config.startup_error_log) {
        let _ = std::fs::write(
            &config.startup_error_log,
            format!("failed to redirect stdio: {}", e),
        );
        smolvm::process::exit_child(1);
    }

    // Close ALL inherited file descriptors from the parent
    unsafe {
        let max_fd = libc::getdtablesize();
        for fd in 3..max_fd {
            libc::close(fd);
        }
    }

    // Open storage and overlay disks
    let storage_disk = match smolvm::storage::StorageDisk::open_or_create_at(
        &config.storage_disk_path,
        config.storage_size_gb,
    ) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::write(
                &config.startup_error_log,
                format!("failed to open storage disk: {}", e),
            );
            smolvm::process::exit_child(1);
        }
    };

    let overlay_disk = match smolvm::storage::OverlayDisk::open_or_create_at(
        &config.overlay_disk_path,
        config.overlay_size_gb,
    ) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::write(
                &config.startup_error_log,
                format!("failed to open overlay disk: {}", e),
            );
            smolvm::process::exit_child(1);
        }
    };

    // Start DNS filter listener if configured
    let dns_filter_socket_path = if let Some(ref hosts) = config.dns_filter_hosts {
        if !hosts.is_empty() {
            let socket_path = config
                .vsock_socket
                .parent()
                .unwrap_or(std::path::Path::new("/tmp"))
                .join("dns-filter.sock");
            if let Err(e) = smolvm::dns_filter_listener::start(&socket_path, hosts.clone()) {
                tracing::warn!(error = %e, "failed to start DNS filter listener");
                None
            } else {
                Some(socket_path)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Launch the VM (blocks until exit)
    let disks = VmDisks {
        storage: &storage_disk,
        overlay: Some(&overlay_disk),
    };

    // Egress telemetry lands in the per-VM dir (the vsock socket's parent), the
    // same dir the node API resolves from the machine name — mirror the engine's
    // internal_boot path so `smol ls`/inspect surface egressBytes too.
    let egress_telemetry_path = config.vsock_socket.parent().map(|dir| dir.join("egress"));

    let result = launch_agent_vm(&LaunchConfig {
        rootfs_path: &config.rootfs_path,
        disks: &disks,
        vsock_socket: &config.vsock_socket,
        console_log: config.console_log.as_deref(),
        egress_telemetry: egress_telemetry_path.as_deref(),
        mounts: &config.mounts,
        port_mappings: &config.ports,
        resources: config.resources,
        ssh_agent_socket: config.ssh_agent_socket.as_deref(),
        dns_filter_socket: dns_filter_socket_path.as_deref(),
        cuda_socket: None,
        packed_layers_dir: config.packed_layers_dir.as_deref(),
        extra_disks: &config.extra_disks,
        dns_filter_enabled: config
            .dns_filter_hosts
            .as_ref()
            .is_some_and(|hosts| !hosts.is_empty()),
        egress_refresh_hosts: config.dns_filter_hosts.clone(),
    });

    if let Err(ref e) = result {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.startup_error_log)
            .and_then(|mut file| {
                use std::io::Write;
                writeln!(file, "{e}")
            });
    }

    smolvm::process::exit_child(1);
}
