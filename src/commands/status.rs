//! smol machine status — show machine details.

use super::common;
use clap::Args;
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct StatusCmd {
    /// Machine name (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Show cloud machine details (by name or ID). Usually unnecessary — a
    /// machine's location is resolved automatically; equivalent to `cloud/`.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl StatusCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            self.name = Some(handle);
            return self.run_cloud();
        }
        let name = handle;

        let manager = common::get_manager(&name)?;
        let is_running = manager.try_connect_existing().is_some();

        // Try to get record from DB for richer info
        let record = SmolvmDb::open()
            .ok()
            .and_then(|db| db.get_vm(&name).ok().flatten());

        if self.json {
            let obj = serde_json::json!({
                "name": name,
                "running": is_running,
                "pid": manager.child_pid(),
                "cpus": record.as_ref().map(|r| r.cpus),
                "memory_mib": record.as_ref().map(|r| r.mem),
                "image": record.as_ref().and_then(|r| r.image.clone()),
                "network": record.as_ref().map(|r| r.network),
            });
            println!("{}", serde_json::to_string_pretty(&obj)?);
        } else if is_running {
            println!("Machine '{}': running", name);
            if let Some(pid) = manager.child_pid() {
                println!("  PID: {}", pid);
            }
            if let Some(ref r) = record {
                println!("  CPUs: {}, Memory: {} MiB", r.cpus, r.mem);
                if let Some(ref img) = r.image {
                    println!("  Image: {}", img);
                }
                if r.network {
                    println!("  Network: enabled");
                }
                if !r.mounts.is_empty() {
                    println!("  Mounts: {}", r.mounts.len());
                }
                if !r.ports.is_empty() {
                    println!("  Ports: {}", r.ports.len());
                }
            }
        } else {
            println!("Machine '{}': not running", name);
        }

        if is_running {
            manager.detach();
        }
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        let json_output = self.json;
        super::cloud::run_cloud_command(self.name, |http, endpoint, id| async move {
            let resp = http
                .get(format!("{}/v1/machines/{}", endpoint, id))
                .send()
                .await?;

            match resp.status().as_u16() {
                200 => {
                    let machine: super::cloud::CloudMachine = resp.json().await?;
                    if json_output {
                        println!("{}", serde_json::to_string_pretty(&machine)?);
                    } else {
                        let source = machine
                            .source
                            .as_ref()
                            .and_then(|s| s.reference.as_deref())
                            .unwrap_or("-");
                        let cpus = machine.resources.as_ref().and_then(|r| r.cpus).unwrap_or(0);
                        let mem = machine
                            .resources
                            .as_ref()
                            .and_then(|r| r.memory_mb)
                            .unwrap_or(0);
                        let created = machine.created_at.as_deref().unwrap_or("-");
                        let updated = machine.updated_at.as_deref().unwrap_or("-");

                        println!(
                            "Machine '{}' ({}): {}",
                            machine.name.as_deref().unwrap_or("-"),
                            id,
                            machine.state
                        );
                        println!("  Source:  {}", source);
                        println!("  CPUs:    {}", cpus);
                        println!("  Memory:  {} MiB", mem);
                        println!("  Created: {}", created);
                        println!("  Updated: {}", updated);
                    }
                }
                404 => anyhow::bail!("machine '{}' not found", id),
                _ => {
                    super::cloud::check_response(resp, "get machine status").await?;
                }
            }
            Ok(())
        })
    }
}
