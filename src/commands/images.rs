//! smol images — list a machine's cached images and storage usage.
//!
//! Mirrors the engine's `machine images` over the public `smolvm` library:
//! connects to the machine's agent (starting it transiently if needed) and
//! queries storage status + the cached image list.

use super::common::format_bytes;
use clap::Args;
use smolvm::agent::{AgentClient, AgentManager};
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct ImagesCmd {
    /// Machine to query
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Output in JSON format
    #[arg(long)]
    pub json: bool,
}

impl ImagesCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let db = SmolvmDb::open()?;
        let record = db
            .get_vm(&self.name)?
            .ok_or_else(|| anyhow::anyhow!("machine '{}' not found", self.name))?;

        let manager =
            AgentManager::for_vm_with_sizes(&self.name, record.storage_gb, record.overlay_gb)?;

        // Reuse the running agent if present; otherwise start transiently and
        // stop it again so a read-only query never leaves the VM running.
        let started_for_query = if manager.try_connect_existing().is_some() {
            manager.detach();
            false
        } else {
            eprintln!("Starting machine '{}' to query storage...", self.name);
            manager.start()?;
            true
        };

        let mut client = AgentClient::connect_with_retry(manager.vsock_socket())?;
        let status = client.storage_status()?;
        let images = client.list_images()?;

        if self.json {
            let output = serde_json::json!({
                "storage": {
                    "total_bytes": status.total_bytes,
                    "used_bytes": status.used_bytes,
                    "layer_count": status.layer_count,
                    "image_count": status.image_count,
                },
                "images": images,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("Storage Usage:");
            println!("  Total:  {}", format_bytes(status.total_bytes));
            println!("  Used:   {}", format_bytes(status.used_bytes));
            println!("  Layers: {}", status.layer_count);
            println!();

            if images.is_empty() {
                println!("No cached images.");
            } else {
                println!("Cached Images:");
                println!("{:<40} {:>10} {:>8}", "IMAGE", "SIZE", "LAYERS");
                println!("{}", "-".repeat(60));
                for image in &images {
                    let name = if image.reference.len() > 38 {
                        format!("{}...", &image.reference[..35])
                    } else {
                        image.reference.clone()
                    };
                    println!(
                        "{:<40} {:>10} {:>8}",
                        name,
                        format_bytes(image.size),
                        image.layer_count
                    );
                }
                println!();
                println!("Total: {} images", images.len());
            }
        }

        if started_for_query {
            let _ = manager.stop();
        }
        Ok(())
    }
}
