//! smol file — the Smolfile noun: scaffold and run a declarative machine.
//!
//! The `Smolfile` is smol's declarative single-machine spec (image, ports,
//! mounts, resources). Everything that operates on it hangs off this one noun
//! (`smol file <verb>`), the same noun-first shape as `smol machine <verb>`.

use clap::{Args, Subcommand};

/// Work with a Smolfile: scaffold, bring up, tear down.
#[derive(Args, Debug)]
pub struct FileCmd {
    #[command(subcommand)]
    pub command: FileSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum FileSubcommand {
    /// Create a Smolfile in the current directory
    Init(crate::commands::init::InitCmd),

    /// Start the machine defined by a Smolfile
    Up(crate::commands::up::UpCmd),

    /// Stop the machine started by `smol file up`
    Down(crate::commands::down::DownCmd),
}

impl FileCmd {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            FileSubcommand::Init(cmd) => cmd.run(),
            FileSubcommand::Up(cmd) => cmd.run(),
            FileSubcommand::Down(cmd) => cmd.run(),
        }
    }
}
