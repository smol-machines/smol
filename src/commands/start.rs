//! smol machine start — start a machine.

use clap::{builder::TypedValueParser, Args};
use smolvm::agent::AgentManager;
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

    /// Start as a branch source: back guest RAM with a memfd (CoW-cloneable) and
    /// expose a control socket so the machine can be branched later.
    #[arg(long = "branchable", visible_alias = "forkable")]
    pub forkable: bool,

    /// Route outbound TCP through a host interceptor on this launch.
    /// Set SMOLVM_INTERCEPTOR_TOKEN to 64 hex digits.
    #[arg(long, value_name = "ADDR")]
    pub egress_interceptor: Option<std::net::SocketAddr>,

    #[arg(
        long,
        env = "SMOLVM_INTERCEPTOR_TOKEN",
        hide = true,
        hide_env_values = true,
        value_parser = clap::builder::StringValueParser::new().map(smolvm::secrets::Secret::new)
    )]
    pub egress_interceptor_token: Option<smolvm::secrets::Secret>,
}

impl StartCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        // Location is an attribute of the machine, not a command path: resolve it
        // from the reference (+ optional --local/--cloud), then route.
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            anyhow::ensure!(
                self.egress_interceptor.is_none(),
                "--egress-interceptor is only supported for local machines"
            );
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
                    anyhow::ensure!(
                        self.egress_interceptor.is_none(),
                        "--egress-interceptor requires a named machine record"
                    );
                    // Start a bare default VM
                    return self.start_default();
                }
                anyhow::bail!(
                    "machine '{}' not found. Create it first with: smol machine create --name {}",
                    name,
                    name
                );
            }
        };

        let interceptor = self
            .egress_interceptor
            .map(|addr| {
                smolvm::embedded::interceptor_endpoint(
                    &addr.to_string(),
                    self.egress_interceptor_token
                        .as_ref()
                        .ok_or_else(|| {
                            smolvm::Error::config(
                                "egress interceptor",
                                "set SMOLVM_INTERCEPTOR_TOKEN to 64 random hex digits",
                            )
                        })?
                        .expose(),
                )
            })
            .transpose()?;
        if interceptor.is_some() {
            anyhow::ensure!(
                !self.forkable && !record.forkable_on_start(),
                "external interception does not support branch launches"
            );
        }

        // Use the engine's persistent lifecycle for image preparation, init,
        // workload launch and checkpoint recovery. In particular, init belongs
        // inside the image's filesystem and runs only until it succeeds once.
        let proxy_options = smolvm::cli::proxy_opts::ProxyOpts::default();
        let proxy = proxy_options.resolved_proxy()?;
        let no_proxy = proxy_options.no_proxy();
        let fork = if self.forkable {
            smolvm::cli::vm_common::forkable_launch()
        } else {
            smolvm::cli::vm_common::ForkLaunch::default()
        };
        smolvm::cli::vm_common::start_vm_named(
            &name,
            proxy.as_deref(),
            no_proxy.as_deref(),
            false,
            fork,
            smolvm::cli::vm_common::StartOptions {
                external_interceptor: interceptor,
                ..Default::default()
            },
        )?;
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        let forkable = self.forkable;
        super::cloud::run_cloud_command(self.name, move |http, endpoint, id| async move {
            eprintln!("Starting {}...", id);
            let mut req = http.post(format!("{}/v1/machines/{}/start", endpoint, id));
            req = req.timeout(std::time::Duration::from_secs(
                super::common::HTTP_LONG_OPERATION_TIMEOUT_SECS,
            ));
            if forkable {
                // Start with cloneable live RAM so it can be branched later.
                req = req.query(&[("forkable", "true")]);
            }
            let resp = req.send().await?;

            match resp.status().as_u16() {
                200 => {
                    let machine: super::cloud::CloudMachine = resp.json().await?;
                    match machine.ready {
                        Some(true) => {
                            eprintln!("Machine {}: ready for work", id);
                        }
                        Some(false) if machine.state == "started" => {
                            eprintln!(
                                "Machine {}: VM launched (state: started); not ready for work yet",
                                id
                            );
                            eprintln!(
                                "  The guest agent is still starting; wait for ready=true before exec or connect."
                            );
                        }
                        _ if machine.state == "started" => {
                            eprintln!(
                                "Machine {}: VM launched (state: started); readiness not reported",
                                id
                            );
                        }
                        _ => eprintln!("Machine {}: {}", id, machine.state),
                    }
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
        // Open for launch so a never-started default machine gets its disks.
        let manager = AgentManager::new_default_with_sizes(None, None)?;

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
