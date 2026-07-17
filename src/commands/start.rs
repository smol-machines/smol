//! smol machine start — start a machine.

use clap::Args;
use smolvm::agent::AgentManager;
use smolvm::config::RecordState;
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct StartCmd {
    /// Machine to start (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Start a cloud machine (by name or ID). Usually unnecessary — a machine's
    /// location is resolved automatically; equivalent to a `cloud/` prefix.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Start as a fork base: back guest RAM with a memfd (CoW-cloneable) and
    /// expose a control socket so the machine can be forked with `smol machine fork`.
    #[arg(long)]
    pub forkable: bool,
}

impl StartCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        // Location is an attribute of the machine, not a command path: resolve it
        // from the reference (+ optional --local/--cloud), then route.
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            self.name = Some(handle);
            return self.run_cloud();
        }
        // `route` already applied the `default` fallback and stripped any prefix.
        let name = handle;

        // Try named VM from database first
        let db = SmolvmDb::open()?;
        let record = match db.get_vm(&name)? {
            Some(r) => r,
            None => {
                if name == "default" {
                    // Start a bare default VM
                    return self.start_default();
                }
                anyhow::bail!(
                    "machine '{}' not found. Create it first with: smol machine create {}",
                    name,
                    name
                );
            }
        };

        // Check state
        if record.actual_state() == RecordState::Running {
            println!("Machine '{}' already running", name);
            return Ok(());
        }

        let mounts = record.host_mounts();
        let ports = record.port_mappings();
        let mut resources = record.vm_resources();

        // Re-resolve --allow-host egress hosts to fresh CIDRs at start (IPs
        // rotate for CDN-backed services), merging with the stored allowlist.
        if let Some(ref hosts) = record.dns_filter_hosts {
            if !hosts.is_empty() {
                let cidrs = resources.allowed_cidrs.get_or_insert_with(Vec::new);
                for h in hosts {
                    match smolvm::smolfile::resolve_host_to_cidrs(h) {
                        Ok(c) => cidrs.extend(c),
                        Err(e) => {
                            eprintln!("Warning: could not resolve '{h}' for egress policy: {e}")
                        }
                    }
                }
            }
        }

        let manager = AgentManager::for_vm_with_sizes(&name, record.storage_gb, record.overlay_gb)?;

        println!("Starting machine '{}'...", name);

        let mut features = smolvm::agent::LaunchFeatures {
            ssh_agent_socket: if record.ssh_agent {
                Some(std::path::PathBuf::from(
                    std::env::var("SSH_AUTH_SOCK")
                        .map_err(|_| anyhow::anyhow!("SSH_AUTH_SOCK not set"))?,
                ))
            } else {
                None
            },
            dns_filter_hosts: record.dns_filter_hosts.clone(),
            ..Default::default()
        }
        // For a machine created with `--from`, use the .smolmachine's
        // pre-extracted layers instead of pulling. No-op when unset.
        .with_packed_layers(
            &smolvm::agent::machine_layers_cache_dir(&name),
            record.source_smolmachine.as_deref(),
        )?;

        // Fork base: memfd-back guest RAM + expose a control socket so `smol machine fork`
        // can later freeze it as a CoW base. These MUST go on the launch features
        // — the manager forwards SMOLVM_FORKABLE / SMOLVM_CONTROL_SOCKET to the
        // boot subprocess from `features`, not from a (non-inherited) process env.
        if self.forkable {
            features.forkable = true;
            features.control_socket = Some(smolvm::agent::fork::control_socket_path(&name));
        }

        // `smol` sets SMOLVM_BOOT_BINARY (its own exe can't serve `_boot-vm`), but
        // `start` DETACHES the machine to persist after we exit. Opt out of the
        // boot subprocess's parent-death watchdog, or the VM would die the moment
        // this command returns (and `smol machine exec`/`fork` would then fail).
        features.watch_parent = Some(false);

        manager.ensure_running_with_full_config(mounts, ports, resources, features)?;

        let pid = manager.child_pid();

        // Run init commands with a per-command timeout. Init sees the record's
        // env plus its resolved secrets (resolved once, host-side; never stored).
        const INIT_TIMEOUT_SECS: u64 = 120;
        if !record.init.is_empty() {
            let mut init_env = record.env.clone();
            init_env.extend(super::common::resolve_record_secrets(&record.secret_refs)?);
            println!("Running {} init command(s)...", record.init.len());
            for (i, cmd) in record.init.iter().enumerate() {
                // Fresh connection per command — thread takes ownership and we reconnect each time.
                let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
                let argv = vec!["sh".into(), "-c".into(), cmd.clone()];
                let env = init_env.clone();
                let workdir = record.workdir.clone();
                let (tx, rx) = std::sync::mpsc::channel();

                // Run exec on a separate thread so we can enforce a timeout.
                let cmd_clone = cmd.clone();
                std::thread::spawn(move || {
                    let result = client.vm_exec(argv, env, workdir, None, None);
                    // Ignore send error — receiver may have timed out.
                    let _ = tx.send(result);
                });

                let result = rx
                    .recv_timeout(std::time::Duration::from_secs(INIT_TIMEOUT_SECS))
                    .map_err(|_| {
                        if let Err(e) = manager.stop() {
                            eprintln!("Warning: failed to stop machine after init timeout: {}", e);
                        }
                        anyhow::anyhow!(
                            "init[{}] timed out after {}s: {:?}",
                            i,
                            INIT_TIMEOUT_SECS,
                            cmd_clone
                        )
                    })?
                    .inspect_err(|_| {
                        if let Err(stop_err) = manager.stop() {
                            eprintln!(
                                "Warning: failed to stop machine after init error: {}",
                                stop_err
                            );
                        }
                    })?;

                let (exit_code, _stdout, stderr) = result;
                if exit_code != 0 {
                    if let Err(e) = manager.stop() {
                        eprintln!("Warning: failed to stop machine after init failure: {}", e);
                    }
                    let stderr_str = String::from_utf8_lossy(&stderr);
                    anyhow::bail!(
                        "init[{}] failed (exit {}): {}",
                        i,
                        exit_code,
                        stderr_str.trim()
                    );
                }
            }
        }

        // Pull image if configured — unless the machine was created `--from` a
        // .smolmachine, in which case its layers are already extracted locally.
        if record.source_smolmachine.is_none() {
            if let Some(ref image) = record.image {
                let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
                println!("Pulling {}...", image);
                client.pull_with_registry_config(image)?;
            }
        }

        // Launch the machine's workload, mirroring the engine's `machine start`:
        // an image machine runs its (entrypoint, cmd) as a detached container
        // (empty command → the agent resolves the image's own ENTRYPOINT+CMD);
        // a bare machine execs it directly. Without this, a golden created with
        // `smol machine create ... -- <workload>` silently never ran it — which
        // also left CUDA fork goldens with nothing to fork.
        {
            let mut exec_env = record.env.clone();
            exec_env.extend(super::common::resolve_record_secrets(&record.secret_refs)?);
            let mut cmd = record.entrypoint.clone();
            cmd.extend(record.cmd.clone());
            if let Some(ref img) = record.image {
                // Positional virtiofs tags, same rule as the engine (smolvm{i}).
                let bindings: Vec<(String, String, bool)> = record
                    .mounts
                    .iter()
                    .enumerate()
                    .map(|(i, (_host, target, ro))| {
                        (
                            smolvm::data::storage::HostMount::mount_tag(i),
                            target.clone(),
                            *ro,
                        )
                    })
                    .collect();
                let bg = smolvm::agent::RunConfig::new(img, cmd)
                    .with_env(exec_env)
                    .with_workdir(record.workdir.clone())
                    .with_user(record.user.clone())
                    .with_mounts(bindings)
                    .with_persistent_overlay(Some(name.clone()));
                let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
                if let Err(e) = client.run_container_detached(bg) {
                    if let Err(stop_err) = manager.stop() {
                        eprintln!(
                            "Warning: failed to stop machine after workload launch failure: {}",
                            stop_err
                        );
                    }
                    anyhow::bail!("start workload: {}", e);
                }
            } else if !cmd.is_empty() {
                let mut client = smolvm::AgentClient::connect_with_retry(manager.vsock_socket())?;
                let (exit_code, _stdout, stderr) =
                    client.vm_exec(cmd, exec_env, record.workdir.clone(), None, None)?;
                if exit_code != 0 {
                    eprintln!(
                        "workload exited with code {}: {}",
                        exit_code,
                        String::from_utf8_lossy(&stderr).trim()
                    );
                }
            }
        }

        println!("Machine '{}' running (PID: {})", name, pid.unwrap_or(0));

        // Persist running state
        let pid_start_time = pid.and_then(smolvm::process::process_start_time);
        if let Err(e) = db.update_vm(&name, |r| {
            r.state = RecordState::Running;
            r.pid = pid;
            r.pid_start_time = pid_start_time;
        }) {
            eprintln!("Warning: failed to persist VM state: {}", e);
        }

        manager.detach();
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        let forkable = self.forkable;
        super::cloud::run_cloud_command(self.name, move |http, endpoint, id| async move {
            eprintln!("Starting {}...", id);
            let mut req = http.post(format!("{}/v1/machines/{}/start", endpoint, id));
            if forkable {
                // Start as a live-RAM fork base (golden) so it can be `smol machine fork --cloud`-ed.
                req = req.query(&[("forkable", "true")]);
            }
            let resp = req.send().await?;

            match resp.status().as_u16() {
                200 => {
                    let machine: super::cloud::CloudMachine = resp.json().await?;
                    eprintln!("Machine {}: {}", id, machine.state);
                    if let Some(url) = machine.url.as_deref() {
                        println!("{url}");
                    }
                }
                404 => anyhow::bail!("machine '{}' not found", id),
                _ => {
                    super::cloud::check_response(resp, "start machine").await?;
                }
            }
            Ok(())
        })
    }

    fn start_default(&self) -> anyhow::Result<()> {
        let manager = AgentManager::new_default()?;

        if manager.try_connect_existing().is_some() {
            println!("Machine 'default' already running");
            manager.detach();
            return Ok(());
        }

        println!("Starting machine 'default'...");
        manager.ensure_running()?;

        println!(
            "Machine 'default' running (PID: {})",
            manager.child_pid().unwrap_or(0)
        );

        manager.detach();
        Ok(())
    }
}
