//! smol file down — stop the machine started by `smol file up`.

use clap::Args;
use smolvm::agent::AgentManager;
use smolvm::config::{RecordState, SmolvmConfig};
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct DownCmd {
    /// Smolfile path (default: ./Smolfile)
    #[arg(short = 's', long, value_name = "PATH")]
    pub smolfile: Option<PathBuf>,
}

impl DownCmd {
    pub fn run(self) -> anyhow::Result<()> {
        // Derive machine name from directory name (same as `up`)
        let name = super::common::name_from_cwd()?;

        let mut config = SmolvmConfig::load()?;

        match config.get_vm(&name) {
            Some(record) => {
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
                println!("No machine '{}' found (was it started with 'smol file up'?)", name);
            }
        }

        Ok(())
    }
}
