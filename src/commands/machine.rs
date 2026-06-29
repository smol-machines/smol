//! smol machine — per-machine maintenance and introspection.
//!
//! Groups the lower-frequency machine-management verbs (image cache, disk
//! settings, supervision, data-dir lookup) under one noun so the top-level
//! help stays focused on the daily-driver lifecycle commands.

use clap::{Args, Subcommand};

/// Per-machine maintenance and introspection.
#[derive(Args, Debug)]
pub struct MachineCmd {
    #[command(subcommand)]
    pub command: MachineSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum MachineSubcommand {
    /// List a machine's cached images and storage usage
    Images(crate::commands::images::ImagesCmd),

    /// Remove a machine's unused images and layers to free disk space
    Prune(crate::commands::prune::PruneCmd),

    /// Modify settings on a stopped machine (mounts, ports, resources, disks)
    Update(crate::commands::update::UpdateCmd),

    /// Supervise a machine with health checks and a restart policy
    Monitor(crate::commands::monitor::MonitorCmd),

    /// Print the on-disk data directory path for a machine
    #[command(name = "data-dir")]
    DataDir(crate::commands::data_dir::DataDirCmd),
}

impl MachineCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            MachineSubcommand::Images(cmd) => cmd.run(),
            MachineSubcommand::Prune(cmd) => cmd.run(),
            MachineSubcommand::Update(cmd) => cmd.run(),
            MachineSubcommand::Monitor(cmd) => cmd.run(),
            MachineSubcommand::DataDir(cmd) => cmd.run(),
        }
    }
}
