//! smol file up — start a machine from a Smolfile.

use clap::Args;
use smolvm::agent::{AgentManager, LaunchFeatures, VmResources};
use smolvm::config::{RecordState, SmolvmConfig, VmRecord};
use smolvm::data::network::PortMapping;
use smolvm::data::storage::HostMount;
use smolvm::smolfile::{self, Smolfile};
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct UpCmd {
    /// Run in background
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Smolfile path (default: ./Smolfile)
    #[arg(short = 's', long, value_name = "PATH")]
    pub smolfile: Option<PathBuf>,
}

impl UpCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let smolfile_path = self.smolfile.unwrap_or_else(|| PathBuf::from("Smolfile"));

        if !smolfile_path.exists() {
            anyhow::bail!(
                "Smolfile not found at '{}'. Run 'smol file init' to create one.",
                smolfile_path.display()
            );
        }

        let sf: Smolfile = smolfile::load(&smolfile_path)?;

        // Derive machine name from directory name
        let name = super::common::name_from_cwd()?;

        let mut config = SmolvmConfig::load()?;

        // Check if already running
        if let Some(record) = config.get_vm(&name) {
            if record.actual_state() == RecordState::Running {
                println!("Machine '{}' already running", name);
                return Ok(());
            }
        }

        // Resolve [dev] fields, falling back to top-level
        let dev = sf.dev.unwrap_or_default();

        // Ports: [dev].ports > top-level ports
        let port_strs = if !dev.ports.is_empty() {
            &dev.ports
        } else {
            &sf.ports
        };
        let mounts_strs = if !dev.volumes.is_empty() {
            &dev.volumes
        } else {
            &sf.volumes
        };
        let init_cmds = if !dev.init.is_empty() {
            &dev.init
        } else {
            &sf.init
        };

        let mounts = HostMount::parse(mounts_strs)?;
        let ports: Vec<PortMapping> = port_strs
            .iter()
            .map(|s| PortMapping::parse(s).map_err(|e| anyhow::anyhow!("{}", e)))
            .collect::<Result<Vec<_>, _>>()?;

        let cpus = sf.cpus.unwrap_or(4);
        let mem = sf.memory.unwrap_or(8192);
        let net = sf.net.unwrap_or(false) || !ports.is_empty();
        let gpu = sf.gpu.unwrap_or(false);
        let gpu_vram_mib = sf.gpu_vram;

        // Resolve [network] section for egress policy
        let network_config = sf.network.unwrap_or_default();
        let mut allowed_cidrs: Vec<String> = Vec::new();
        for host in &network_config.allow_hosts {
            let cidrs =
                smolfile::resolve_host_to_cidrs(host).map_err(|e| anyhow::anyhow!("{}", e))?;
            allowed_cidrs.extend(cidrs);
        }
        for cidr in &network_config.allow_cidrs {
            let parsed = smolfile::parse_cidr(cidr).map_err(|e| anyhow::anyhow!("{}", e))?;
            allowed_cidrs.push(parsed);
        }
        let net = net || !allowed_cidrs.is_empty();
        let dns_filter_hosts: Option<Vec<String>> = if network_config.allow_hosts.is_empty() {
            None
        } else {
            Some(network_config.allow_hosts)
        };

        let resources = VmResources {
            cpus,
            memory_mib: mem,
            network: net,
            cuda: false,
            storage_gib: sf.storage,
            overlay_gib: sf.overlay,
            allowed_cidrs: if allowed_cidrs.is_empty() {
                None
            } else {
                Some(allowed_cidrs)
            },
            network_backend: None,
            gpu,
            gpu_vram_mib,
            rosetta: false,
            cuda: false,
            dns: None,
        };

        // Resolve [auth] for SSH agent
        let ssh_agent = sf.auth.as_ref().and_then(|a| a.ssh_agent).unwrap_or(false);
        let ssh_agent_socket = if ssh_agent {
            match std::env::var("SSH_AUTH_SOCK") {
                Ok(path) => Some(std::path::PathBuf::from(path)),
                Err(_) => {
                    anyhow::bail!("SSH_AUTH_SOCK not set. Start an SSH agent with: eval $(ssh-agent) && ssh-add");
                }
            }
        } else {
            None
        };

        // Merge env: top-level + [dev]
        let mut all_env = sf.env.clone();
        all_env.extend(dev.env.clone());
        let env = smolvm::util::parse_env_list(&all_env);

        // Workdir: [dev].workdir > top-level workdir
        let workdir = dev.workdir.or(sf.workdir);

        // Create or update record
        let ports_tuples = PortMapping::to_tuples(&ports);
        let mounts_tuples: Vec<(String, String, bool)> = mounts
            .iter()
            .map(|m| {
                (
                    m.source.to_string_lossy().to_string(),
                    m.target.to_string_lossy().to_string(),
                    m.read_only,
                )
            })
            .collect();

        if config.get_vm(&name).is_none() {
            let mut record =
                VmRecord::new(name.clone(), cpus, mem, mounts_tuples, ports_tuples, net);
            record.image = sf.image.clone();
            record.env = env.clone();
            record.workdir = workdir.clone();
            record.init = init_cmds.clone();
            record.entrypoint = sf.entrypoint.clone();
            record.cmd = sf.cmd.clone();
            record.storage_gb = sf.storage;
            record.overlay_gb = sf.overlay;
            record.ssh_agent = ssh_agent;
            record.dns_filter_hosts = dns_filter_hosts.clone();

            // Wire [health] into record
            if let Some(ref h) = sf.health {
                if !h.exec.is_empty() {
                    record.health_cmd = Some(h.exec.clone());
                }
                record.health_interval_secs = h
                    .interval
                    .as_ref()
                    .and_then(|s| smolfile::parse_duration_secs(s));
                record.health_timeout_secs = h
                    .timeout
                    .as_ref()
                    .and_then(|s| smolfile::parse_duration_secs(s));
                record.health_retries = h.retries;
                record.health_startup_grace_secs = h
                    .startup_grace
                    .as_ref()
                    .and_then(|s| smolfile::parse_duration_secs(s));
            }

            config.insert_vm(name.clone(), record)?;
        }

        // Start the VM
        let manager = AgentManager::for_vm_with_sizes(&name, sf.storage, sf.overlay)?;

        println!("Starting machine '{}' from Smolfile...", name);

        let features = LaunchFeatures {
            ssh_agent_socket,
            dns_filter_hosts,
            ..Default::default()
        };

        manager.ensure_running_with_full_config(mounts, ports, resources, features)?;

        let pid = manager.child_pid();

        // Run init commands
        if !init_cmds.is_empty() {
            println!("Running {} init command(s)...", init_cmds.len());
            let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
            for (i, cmd) in init_cmds.iter().enumerate() {
                let argv = vec!["sh".into(), "-c".into(), cmd.clone()];
                let (exit_code, _stdout, stderr) =
                    client.vm_exec(argv, env.clone(), workdir.clone(), None, None)?;
                if exit_code != 0 {
                    if let Err(e) = manager.stop() {
                        eprintln!("Warning: failed to stop machine after init failure: {}", e);
                    }
                    anyhow::bail!(
                        "init[{}] failed (exit {}): {}",
                        i,
                        exit_code,
                        String::from_utf8_lossy(&stderr).trim()
                    );
                }
            }
        }

        // Pull image if specified
        if let Some(ref image) = sf.image {
            let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
            print!("Pulling {}...", image);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            client.pull_with_registry_config(image)?;
            println!(" done.");
        }

        // Update DB state
        let pid_start_time = pid.and_then(smolvm::process::process_start_time);
        if let Ok(db) = smolvm::db::SmolvmDb::open() {
            if let Err(e) = db.update_vm(&name, |r| {
                r.state = RecordState::Running;
                r.pid = pid;
                r.pid_start_time = pid_start_time;
            }) {
                eprintln!("Warning: failed to persist VM state: {}", e);
            }
        }

        println!("Machine '{}' running (PID: {})", name, pid.unwrap_or(0));

        manager.detach();
        Ok(())
    }
}
