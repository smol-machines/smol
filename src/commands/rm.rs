//! smol rm — delete a machine.

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
}

impl RmCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let mut config = SmolvmConfig::load()?;

        let record = config
            .get_vm(&self.name)
            .ok_or_else(|| anyhow::anyhow!("machine '{}' not found", self.name))?
            .clone();

        // Stop if running
        if record.actual_state() == smolvm::config::RecordState::Running {
            if let Ok(manager) = AgentManager::for_vm(&self.name) {
                println!("Stopping machine '{}'...", self.name);
                if let Err(e) = manager.stop() {
                    eprintln!("Warning: failed to stop machine: {}", e);
                }
            }
        }

        // Confirm unless --force
        if !self.force {
            eprint!("Delete machine '{}'? [y/N] ", self.name);
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_ok() {
                let input = input.trim().to_lowercase();
                if input != "y" && input != "yes" {
                    println!("Cancelled");
                    return Ok(());
                }
            } else {
                println!("Cancelled");
                return Ok(());
            }
        }

        config.remove_vm(&self.name);

        // Clean up data directory
        let data_dir = smolvm::agent::vm_data_dir(&self.name);
        if data_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                eprintln!("Warning: failed to clean up data directory: {}", e);
            }
        }

        println!("Deleted machine: {}", self.name);
        Ok(())
    }
}
