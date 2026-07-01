//! smol machine stop — stop a machine.

use clap::Args;
use smolvm::agent::AgentManager;
use smolvm::config::{RecordState, SmolvmConfig};

#[derive(Args, Debug)]
pub struct StopCmd {
    /// Machine to stop (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Stop a cloud machine (by name or ID). Usually unnecessary — a machine's
    /// location is resolved automatically; equivalent to a `cloud/` prefix.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl StopCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            self.name = Some(handle);
            return self.run_cloud();
        }
        let name = handle;

        let mut config = SmolvmConfig::load()?;

        // Check config for the named VM
        match config.get_vm(&name) {
            Some(record) => {
                let record = record.clone();
                if record.actual_state() != RecordState::Running {
                    println!("Machine '{}' is not running", name);
                    return Ok(());
                }

                println!("Stopping machine '{}'...", name);
                let manager = AgentManager::for_vm(&name)?;
                manager.stop()?;

                config.update_vm(&name, |r| {
                    r.state = RecordState::Stopped;
                    r.pid = None;
                    r.pid_start_time = None;
                });

                println!("Stopped machine: {}", name);
            }
            None => {
                // Not in config — try to stop a running VM directly
                let manager = if name == "default" {
                    AgentManager::new_default()?
                } else {
                    AgentManager::for_vm(&name)?
                };

                if manager.try_connect_existing().is_some() {
                    println!("Stopping machine '{}'...", name);
                    manager.stop()?;
                    println!("Stopped machine: {}", name);
                } else {
                    println!("Machine '{}' not found or not running", name);
                }
            }
        }

        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        super::cloud::run_cloud_command(self.name, |http, endpoint, id| async move {
            eprintln!("Stopping {}...", id);
            let resp = http
                .post(format!("{}/v1/machines/{}/stop", endpoint, id))
                .send()
                .await?;

            match resp.status().as_u16() {
                200 => {
                    let machine: super::cloud::CloudMachine = resp.json().await?;
                    eprintln!("Machine {}: {}", id, machine.state);
                }
                404 => anyhow::bail!("machine '{}' not found", id),
                status => {
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("stop failed ({}): {}", status, text);
                }
            }
            Ok(())
        })
    }
}
