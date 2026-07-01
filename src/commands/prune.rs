//! smol prune — remove a machine's unused images and layers to free disk.
//!
//! Mirrors the engine's `machine prune` over the public `smolvm` library.
//! Regular prune reclaims only unreferenced layers (safe while running);
//! `--all` purges the cache and so requires a stopped machine. An image-backed
//! machine keeps the cached image it needs to restart even under `--all`.

use super::common::format_bytes;
use clap::Args;
use smolvm::agent::{AgentClient, AgentManager};
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct PruneCmd {
    /// Machine to prune
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Show what would be removed without actually removing
    #[arg(long)]
    pub dry_run: bool,

    /// Remove all cached images (not just unreferenced layers)
    #[arg(long)]
    pub all: bool,
}

impl PruneCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let db = SmolvmDb::open()?;
        let record = db
            .get_vm(&self.name)?
            .ok_or_else(|| anyhow::anyhow!("machine '{}' not found", self.name))?;

        let manager =
            AgentManager::for_vm_with_sizes(&self.name, record.storage_gb, record.overlay_gb)?;

        // Regular prune (unreferenced layers) is safe on a running VM. `--all`
        // deletes manifests for layers that may be in active use, so require a
        // stop first.
        let already_running = manager.try_connect_existing().is_some();
        let started_for_prune;
        if already_running && self.all {
            manager.detach();
            anyhow::bail!(
                "cannot prune --all while machine '{}' is running. Stop it first with: \
                 smol machine stop --name {}",
                self.name,
                self.name
            );
        } else if already_running {
            started_for_prune = false;
            manager.detach();
        } else {
            eprintln!("Starting machine...");
            manager.start()?;
            started_for_prune = true;
        }

        let mut client = AgentClient::connect_with_retry(manager.vsock_socket())?;

        if self.all {
            let images = client.list_images()?;
            if images.is_empty() {
                println!("No cached images to remove.");
            } else if record.image.is_some() {
                // An image-backed machine needs its cached image to restart, so
                // keep the cache and reclaim only unreferenced layers.
                let total_size: u64 = images.iter().map(|i| i.size).sum();
                if self.dry_run {
                    let would_free = client.garbage_collect(true, false)?;
                    println!(
                        "Machine '{}' is image-backed: would keep {} cached image(s) ({}) it \
                         needs to restart, and free {} of unreferenced layers.",
                        self.name,
                        images.len(),
                        format_bytes(total_size),
                        format_bytes(would_free)
                    );
                } else {
                    let freed = client.garbage_collect(false, false)?;
                    println!(
                        "Kept {} cached image(s) in use by machine '{}'; freed {} of \
                         unreferenced layers.",
                        images.len(),
                        self.name,
                        format_bytes(freed)
                    );
                    eprintln!(
                        "(--all keeps images a machine needs to restart; to reclaim everything: \
                         smol machine rm --name {})",
                        self.name
                    );
                }
            } else {
                // Bare VM: nothing depends on the cache, so purge all.
                let total_size: u64 = images.iter().map(|i| i.size).sum();
                if self.dry_run {
                    println!("Would remove {} images ({})", images.len(), format_bytes(total_size));
                    for image in &images {
                        println!(
                            "  - {} ({}, {} layers)",
                            image.reference,
                            format_bytes(image.size),
                            image.layer_count
                        );
                    }
                } else {
                    println!("Removing all cached images...");
                    let freed = client.garbage_collect(false, true)?;
                    println!("Removed {} images, freed {}", images.len(), format_bytes(freed));
                }
            }
        } else if self.dry_run {
            println!("Scanning for unreferenced layers...");
            let would_free = client.garbage_collect(true, false)?;
            if would_free > 0 {
                println!("Would free {} of unreferenced layers", format_bytes(would_free));
            } else {
                println!("No unreferenced layers to remove.");
            }
        } else {
            println!("Removing unreferenced layers...");
            let freed = client.garbage_collect(false, false)?;
            if freed > 0 {
                println!("Freed {}", format_bytes(freed));
            } else {
                println!("No unreferenced layers to remove.");
            }
        }

        if started_for_prune {
            let _ = manager.stop();
        }
        Ok(())
    }
}
