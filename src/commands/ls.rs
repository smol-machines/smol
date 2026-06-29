//! smol ls — list machines.

use clap::Args;
use smolvm::config::SmolvmConfig;

#[derive(Args, Debug)]
pub struct LsCmd {
    /// Output in JSON format
    #[arg(long)]
    pub json: bool,
}

impl LsCmd {
    pub fn run(&self) -> anyhow::Result<()> {
        let config = SmolvmConfig::load()?;
        let vms: Vec<_> = config.list_vms().collect();

        if vms.is_empty() {
            if self.json {
                println!("[]");
            } else {
                println!("No machines found");
            }
            return Ok(());
        }

        if self.json {
            let json_vms: Vec<_> = vms
                .iter()
                .map(|(name, record)| {
                    serde_json::json!({
                        "name": name,
                        "state": record.actual_state().to_string(),
                        "cpus": record.cpus,
                        "memory_mib": record.mem,
                        "pid": record.pid,
                        "network": record.network,
                        "mounts": record.mounts.len(),
                        "ports": record.ports.len(),
                        "image": record.image,
                        "ephemeral": record.ephemeral,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_vms)?);
        } else {
            println!(
                "{:<20} {:<12} {:>5} {:>10} {:>7} {:>7}",
                "NAME", "STATE", "CPUS", "MEMORY", "MOUNTS", "PORTS"
            );
            println!("{}", "-".repeat(73));

            for (name, record) in vms {
                let state_display = if record.ephemeral {
                    format!("{} (eph)", record.actual_state())
                } else {
                    record.actual_state().to_string()
                };
                println!(
                    "{:<20} {:<12} {:>5} {:>10} {:>7} {:>7}",
                    truncate(name, 18),
                    state_display,
                    record.cpus,
                    format!("{} MiB", record.mem),
                    record.mounts.len(),
                    record.ports.len(),
                );
            }
        }

        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max - 3])
    } else {
        s.to_string()
    }
}
