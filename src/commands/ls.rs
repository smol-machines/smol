//! smol machine ls — list machines across both backends (local + cloud).
//!
//! A machine is one concept; residency is a `LOCATION` column, not a separate
//! command. Cloud rows are best-effort: when not logged in, local still lists
//! and a dim hint is printed to stderr.

use super::resolve::{self, Location, Target};
use clap::Args;

#[derive(Args, Debug)]
pub struct LsCmd {
    /// Only list local machines
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Only list cloud machines
    #[arg(long)]
    pub cloud: bool,

    /// Output in JSON format
    #[arg(long)]
    pub json: bool,
}

impl LsCmd {
    pub fn run(&self) -> anyhow::Result<()> {
        let target = Target::from_flags(self.local, self.cloud)?;
        let listing = resolve::list_all(target)?;

        if self.json {
            let rows: Vec<_> = listing
                .machines
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "location": m.location.as_str(),
                        "name": m.name,
                        "id": m.id,
                        "state": m.state,
                        "cpus": m.cpus,
                        "memory_mib": m.memory_mib,
                        "source": m.source,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }

        if listing.machines.is_empty() {
            println!("No machines found");
        } else {
            println!(
                "{:<8} {:<20} {:<16} {:<10} {:>5} {:>10} {:<24}",
                "LOCATION", "NAME", "ID", "STATE", "CPUS", "MEMORY", "SOURCE"
            );
            println!("{}", "-".repeat(96));
            for m in &listing.machines {
                let id_short = if m.location == Location::Cloud {
                    truncate(&m.id, 14)
                } else {
                    // Local id == name; don't repeat it in the ID column.
                    "-".to_string()
                };
                println!(
                    "{:<8} {:<20} {:<16} {:<10} {:>5} {:>10} {:<24}",
                    m.location.as_str(),
                    truncate(m.name.as_deref().unwrap_or("(unnamed)"), 18),
                    id_short,
                    truncate(&m.state, 10),
                    m.cpus.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                    m.memory_mib.map(|mb| format!("{mb} MiB")).unwrap_or_else(|| "-".into()),
                    truncate(m.source.as_deref().unwrap_or("-"), 22),
                );
            }
        }

        // Best-effort cloud: a plain `smol machine ls` must not fail when logged out, but
        // the user should know cloud wasn't shown.
        if target != Target::Local && !listing.cloud_available {
            eprintln!("# cloud: not logged in (run 'smol auth login' to include cloud machines)");
        }

        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.saturating_sub(3)])
    } else {
        s.to_string()
    }
}
