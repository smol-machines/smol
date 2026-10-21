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
            // A local row is the engine's own object, so `smol` and `smolvm`
            // report a local machine with the same fields; smol adds where the
            // machine lives and, for compatibility, `source`.
            let config = listing
                .machines
                .iter()
                .any(|m| m.location == Location::Local)
                .then(smolvm::config::SmolvmConfig::load)
                .transpose()?;
            let rows: Vec<_> = listing
                .machines
                .iter()
                .map(|m| {
                    let record = config
                        .as_ref()
                        .filter(|_| m.location == Location::Local)
                        .and_then(|config| {
                            config
                                .list_vms()
                                .find(|(name, _)| **name == m.id)
                                .map(|(_, r)| r)
                        });
                    let mut row = match record {
                        Some(record) => smolvm::cli::vm_common::machine_status_json(&m.id, record),
                        None => serde_json::json!({
                            "name": m.name,
                            "state": m.state,
                            "cpus": m.cpus,
                            "memory_mib": m.memory_mib,
                            "image": m.source,
                            // Without this labels are write-only: a caller could
                            // set them and never read them back.
                            "labels": m.labels,
                        }),
                    };
                    if let Some(fields) = row.as_object_mut() {
                        fields.insert("location".into(), serde_json::json!(m.location.as_str()));
                        fields.insert("name".into(), serde_json::json!(m.name));
                        fields.insert("id".into(), serde_json::json!(m.id));
                        fields.insert("source".into(), serde_json::json!(m.source));
                        fields.insert(
                            "movedTo".into(),
                            serde_json::json!(super::moved::from_labels(&m.labels).map(
                                |moved| serde_json::json!({ "name": moved.name, "id": moved.id })
                            )),
                        );
                    }
                    row
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
                // A local machine that moved shows where it went instead of
                // its paused state.
                let moved = super::moved::from_labels(&m.labels);
                let state = if moved.is_some() {
                    "moved"
                } else {
                    m.state.as_str()
                };
                let source = moved
                    .map(|moved| format!("-> cloud/{}", moved.name))
                    .or_else(|| m.source.clone());
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
                    truncate(state, 10),
                    m.cpus.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                    m.memory_mib
                        .map(|mb| format!("{mb} MiB"))
                        .unwrap_or_else(|| "-".into()),
                    truncate(source.as_deref().unwrap_or("-"), 22),
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
