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

    /// Run a docker-compose.yml against real Docker inside a microVM
    Compose(commands::compose::ComposeCmd),

    /// Manage CLI configuration
    Config(commands::config::ConfigCmd),

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
        Commands::Machine(cmd) => cmd.run(),
        Commands::File(cmd) => cmd.run(),
        Commands::Pack(cmd) => cmd.run(),
        Commands::Registry(cmd) => cmd.run(),
        Commands::Auth(cmd) => cmd.run(),
        Commands::Cloud(cmd) => cmd.run(),
        Commands::Compose(cmd) => cmd.run(),
        Commands::Config(cmd) => cmd.run(),
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

/// Open a boot disk honoring its on-disk format — mirrors
/// `internal_boot.rs::open_boot_disk`. A `.qcow2` path is a copy-on-write overlay
/// that must be opened as qcow2; opening it as raw exposes the tiny overlay file
/// as the whole device. Keep in sync with internal_boot.rs.
fn open_boot_disk<K: smolvm::storage::DiskType>(
    path: &std::path::Path,
    size_gb: u64,
) -> smolvm::Result<smolvm::storage::VmDisk<K>> {
    if path.extension().and_then(|e| e.to_str()) == Some("qcow2") {
        smolvm::storage::VmDisk::<K>::open_existing_with_format(
            path,
            smolvm::storage::DiskFormat::Qcow2,
        )
    } else {
        smolvm::storage::VmDisk::<K>::open_or_create_at(path, size_gb)
    }
}

/// Run the internal VM boot subprocess.
///
/// Identical to smolvm's `src/cli/internal_boot.rs::run()`.
/// Must be kept in sync when internal_boot.rs changes.
fn boot_vm(config_path: std::path::PathBuf) -> smolvm::Result<()> {
    use smolvm::agent::boot_config::BootConfig;
    use smolvm::agent::{launch_agent_vm, LaunchConfig, VmDisks};

    // Become a session leader (detach from parent's terminal session).
    // POSIX-only; Windows has no process sessions.
    #[cfg(unix)]
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

    // Close ALL inherited file descriptors from the parent. POSIX-only — fd
    // numbers and getdtablesize are a Unix concept; on Windows inherited handles
    // are managed differently and this loop does not apply.
    #[cfg(unix)]
    unsafe {
        let max_fd = libc::getdtablesize();
        for fd in 3..max_fd {
            libc::close(fd);
        }
    }

    // Open storage and overlay disks honoring their on-disk format. A default
    // machine's disks are instant qcow2 CoW overlays (`.qcow2`); opening one as a
    // raw image tells libkrun the tiny (~256 KiB) overlay file is the whole
    // device, so the guest formats a ~256 KiB ext4 and every image pull dies with
    // "no space left on device". Mirrors `internal_boot.rs::open_boot_disk`.
    let storage_disk = match open_boot_disk::<smolvm::storage::Storage>(
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

    let overlay_disk = match open_boot_disk::<smolvm::storage::Overlay>(
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

    // Start the per-VM CUDA-over-vsock host server when requested (mirrors the
    // engine's internal_boot). With it, unmodified CUDA/PyTorch code in the guest
    // runs on the host GPU — the launcher bridges vsock port CUDA to this socket.
    let cuda_socket_path = if config.cuda || config.resources.cuda {
        let path = config
            .vsock_socket
            .parent()
            .unwrap_or(std::path::Path::new("/tmp"))
            .join("cuda.sock");
        match smolvm::cuda_host::start(&path) {
            Ok(()) => Some(path),
            Err(e) => {
                tracing::warn!(error = %e, "failed to start CUDA host server — CUDA disabled");
                None
            }
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
    // internal_boot path so `smol machine ls`/inspect surface egressBytes too.
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
        cuda_socket: cuda_socket_path.as_deref(),
        docker_socket: None,
        published_sockets: &[],
        pod_net: None,
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
