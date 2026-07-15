//! smol run — ephemeral VM execution.

use clap::Args;
use smolvm::agent::{AgentClient, AgentManager, LaunchFeatures, RunConfig, VmResources};
use smolvm::data::network::PortMapping;
use smolvm::data::storage::HostMount;
use smolvm::DEFAULT_SHELL_CMD;

#[derive(Args, Debug)]
pub struct RunCmd {
    /// Container image
    #[arg(short = 'I', long, value_name = "IMAGE")]
    pub image: Option<String>,

    /// Command to execute (after --)
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,

    /// Keep stdin open for interactive input
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long)]
    pub tty: bool,

    /// Set environment variable (KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Set working directory
    #[arg(short = 'w', long, value_name = "DIR")]
    pub workdir: Option<String>,

    /// Mount host directory (HOST:GUEST[:ro])
    #[arg(short = 'v', long = "volume", value_name = "HOST:GUEST[:ro]")]
    pub volume: Vec<String>,

    /// Expose port (HOST:GUEST)
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,

    /// Enable outbound network access
    #[arg(long)]
    pub net: bool,

    /// Number of vCPUs
    #[arg(long, value_name = "N")]
    pub cpus: Option<u8>,

    /// Memory in MiB
    #[arg(long, value_name = "MiB")]
    pub mem: Option<u32>,

    /// Enable GPU acceleration (Vulkan via virtio-gpu)
    #[arg(long)]
    pub gpu: bool,

    /// GPU VRAM in MiB (requires --gpu)
    #[arg(long, value_name = "MiB")]
    pub gpu_vram: Option<u32>,
}

impl RunCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let command = strip_separator(&self.command);

        // Require explicit command or -it
        if !self.interactive && !self.tty && command.is_empty() {
            anyhow::bail!(
                "no command specified.\n\
                 Use: smol run --image <IMAGE> -- <command>\n\
                 Or:  smol run -it --image <IMAGE>"
            );
        }

        let mounts = HostMount::parse(&self.volume)?;
        // Virtiofs binding form the agent uses to bind each mount into the
        // container: (tag, guest_target, read_only). The tag is `smolvm{i}` and
        // must match the virtiofs device order libkrun exposes at VM start, i.e.
        // the order of `mounts` passed to `ensure_running_with_full_config`
        // below. Without this the mounts reach the VM but are never bound into
        // the container, so `-v host:/path` doesn't appear inside the image.
        let mount_bindings: Vec<(String, String, bool)> = mounts
            .iter()
            .enumerate()
            .map(|(i, m)| {
                (
                    HostMount::mount_tag(i),
                    m.target.to_string_lossy().into_owned(),
                    m.read_only,
                )
            })
            .collect();
        let ports = self.port.clone();

        let resources = VmResources {
            cpus: self.cpus.unwrap_or(4),
            memory_mib: self.mem.unwrap_or(8192),
            network: self.net || !self.port.is_empty(),
            storage_gib: None,
            overlay_gib: None,
            allowed_cidrs: None,
            network_backend: None,
            gpu: self.gpu,
            gpu_vram_mib: self.gpu_vram,
            rosetta: false,
            cuda: false,
            dns: None,
        };

        // Resolve the image: a registry reference is pulled in-guest, while a
        // local source — a docker/podman `save` archive (`-I ./img.tar`), stdin
        // (`-I -`), or an unpacked rootfs dir — is staged on the host and mounted
        // into the VM via virtiofs (no registry needed). Mirrors `smolvm machine
        // run`'s image handling so the bundled engine's local-image support is
        // exposed through `smol`.
        let (resolved_image, packed_layers_dir) = match self.image.as_deref() {
            Some(img) => {
                use smolvm::data::image_source::{classify, resolve, ResolvedImage};
                match resolve(classify(img))? {
                    ResolvedImage::Registry(reference) => (Some(reference), None),
                    ResolvedImage::Local {
                        reference,
                        packed_layers_dir,
                    } => (Some(reference), Some(packed_layers_dir)),
                }
            }
            None => (None, None),
        };
        let uses_packed_layers = packed_layers_dir.is_some();

        let manager = AgentManager::new_default()?;

        eprintln!("Starting ephemeral machine...");

        let features = LaunchFeatures {
            ssh_agent_socket: None,
            dns_filter_hosts: None,
            packed_layers_dir,
            ..Default::default()
        };

        manager.ensure_running_with_full_config(mounts.clone(), ports, resources, features)?;

        let mut client = AgentClient::connect_with_retry(manager.vsock_socket())?;

        // Propagate host edits under -v mounts into the guest as fsnotify events
        // so inotify-based hot-reload (Vite, webpack, nodemon) fires when a file
        // is changed on the host. Held for the command's lifetime; dropped (which
        // stops the watcher) when this scope exits. No-op without mounts or on a
        // guest kernel that lacks the /proc/smolvm-fsnotify interface.
        let _fs_watcher = if mounts.is_empty() {
            None
        } else {
            smolvm::agent::FsNotifyWatcher::start(manager.vsock_socket().to_path_buf(), &mounts)
        };

        // Registry images pull in-guest; a local image is already staged on the
        // host (mounted via packed_layers_dir), so skip the pull for it.
        if let Some(ref img) = resolved_image {
            if !uses_packed_layers {
                print!("Pulling {}...", img);
                let _ = std::io::Write::flush(&mut std::io::stdout());
                client.pull_with_registry_config(img)?;
                println!(" done.");
            }
        }

        // Resolve command — default to shell for interactive
        let command = if !command.is_empty() {
            command
        } else {
            vec![DEFAULT_SHELL_CMD.to_string()]
        };

        let env = smolvm::util::parse_env_list(&self.env);

        // Execute
        let exit_code = if let Some(ref img) = resolved_image {
            if self.interactive || self.tty {
                let config = RunConfig::new(img, command)
                    .with_env(env)
                    .with_workdir(self.workdir)
                    .with_mounts(mount_bindings.clone())
                    .with_tty(self.tty);
                client.run_interactive(config)?
            } else {
                let config = RunConfig::new(img, command)
                    .with_env(env)
                    .with_workdir(self.workdir)
                    .with_mounts(mount_bindings.clone());
                let (exit_code, stdout, stderr) = client.run_non_interactive(config)?;
                if !stdout.is_empty() {
                    print!("{}", String::from_utf8_lossy(&stdout));
                }
                if !stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&stderr));
                }
                exit_code
            }
        } else {
            // Bare VM mode
            if self.interactive || self.tty {
                client.vm_exec_interactive(command, env, self.workdir, None, self.tty)?
            } else {
                let (exit_code, stdout, stderr) =
                    client.vm_exec(command, env, self.workdir, None, None)?;
                if !stdout.is_empty() {
                    print!("{}", String::from_utf8_lossy(&stdout));
                }
                if !stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&stderr));
                }
                exit_code
            }
        };

        manager.kill();
        std::process::exit(exit_code);
    }
}

/// Strip a leading `--` separator from a command vec if present.
fn strip_separator(args: &[String]) -> Vec<String> {
    if args.first().map(|s| s.as_str()) == Some("--") {
        args[1..].to_vec()
    } else {
        args.to_vec()
    }
}
