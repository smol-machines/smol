//! smol machine rm — delete a machine (local or cloud).

use clap::Args;
use smolvm::agent::AgentManager;
use smolvm::config::SmolvmConfig;

#[derive(Args, Debug)]
pub struct RmCmd {
    /// Machine to delete
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Skip confirmation prompt
    #[arg(long)]
    pub force: bool,

    /// Delete a cloud machine (by name or ID). Usually unnecessary — a machine's
    /// location is resolved automatically; equivalent to a `cloud/` prefix.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl RmCmd {
    pub fn run(self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        // A machine's location is an attribute, not a command path: resolve it
        // from the reference (+ optional --local/--cloud), then route.
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(Some(&self.name), target)?;

        // Confirm once, regardless of where the machine lives.
        if !self.force && !confirm_delete(&handle)? {
            println!("Cancelled");
            return Ok(());
        }

        match location {
            Location::Cloud => run_cloud(handle),
            Location::Local => run_local(&handle),
        }
    }
}

/// Prompt on stderr for a delete; returns true if the user confirmed.
fn confirm_delete(name: &str) -> anyhow::Result<bool> {
    eprint!("Delete machine '{}'? [y/N] ", name);
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_ok() {
        let input = input.trim().to_lowercase();
        Ok(input == "y" || input == "yes")
    } else {
        Ok(false)
    }
}

/// Delete a local machine: stop it if running, drop the record, remove its data.
fn run_local(name: &str) -> anyhow::Result<()> {
    let mut config = SmolvmConfig::load()?;

    let record = config
        .get_vm(name)
        .ok_or_else(|| anyhow::anyhow!("machine '{}' not found", name))?
        .clone();

    // Stop if running
    if record.actual_state() == smolvm::config::RecordState::Running {
        if let Ok(manager) = AgentManager::for_vm(name) {
            println!("Stopping machine '{}'...", name);
            if let Err(e) = manager.stop() {
                eprintln!("Warning: failed to stop machine: {}", e);
            }
        }
    }

    config.remove_vm(name);

    // Clean up data directory
    let data_dir = smolvm::agent::vm_data_dir(name);
    if data_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&data_dir) {
            eprintln!("Warning: failed to clean up data directory: {}", e);
        }
    }

    println!("Deleted machine: {}", name);
    Ok(())
}

/// Delete a cloud machine via the smolfleet control plane
/// (`DELETE /v1/machines/{id}`).
fn run_cloud(name: String) -> anyhow::Result<()> {
    let display_name = name.clone();
    super::cloud::run_cloud_command(Some(name), |http, endpoint, id| async move {
        eprintln!("Deleting {} ({})...", display_name, id);
        let resp = http
            .delete(format!("{}/v1/machines/{}", endpoint, id))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 | 204 => println!("Deleted machine: {}", display_name),
            404 => anyhow::bail!("machine '{}' not found", display_name),
            status => {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("delete failed ({}): {}", status, text);
            }
        }
        Ok(())
    })
}
