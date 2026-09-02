//! smol machine — the machine noun: lifecycle + maintenance.
//!
//! Every machine operation hangs off this one noun (`smol machine <verb>`), so
//! a machine is a single concept whether it lives locally or in the cloud (the
//! verbs resolve residency from the reference; see `resolve.rs`). The top level
//! keeps only the non-machine flows (`run`, the Smolfile `up`/`down`, `pack`,
//! `auth`, `cloud`, `config`).

use clap::{Args, Subcommand};

/// Manage machines (local or cloud): lifecycle + maintenance.
#[derive(Args, Debug)]
pub struct MachineCmd {
    #[command(subcommand)]
    pub command: MachineSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum MachineSubcommand {
    // --- lifecycle ---------------------------------------------------------
    /// Create a persistent machine
    Create(crate::commands::create::CreateCmd),

    /// Start a machine
    Start(crate::commands::start::StartCmd),

    /// Stop a machine
    Stop(crate::commands::stop::StopCmd),

    /// Delete a machine
    #[command(visible_alias = "delete")]
    Rm(crate::commands::rm::RmCmd),

    /// List machines
    #[command(visible_alias = "list")]
    Ls(crate::commands::ls::LsCmd),

    /// Show machine details
    Status(crate::commands::status::StatusCmd),

    /// Execute a command in a running machine
    Exec(crate::commands::exec::ExecCmd),

    /// Open an interactive shell in a machine
    #[command(visible_alias = "sh")]
    Shell {
        /// Machine name (default: "default")
        #[arg(short = 'n', long, value_name = "NAME")]
        name: Option<String>,

        /// Open a shell on a cloud machine (by name or ID). Usually unnecessary
        /// — a machine's location is resolved automatically; equivalent to a
        /// `cloud/` prefix.
        #[arg(long)]
        cloud: bool,

        /// Force a local machine. Equivalent to a `local/` prefix.
        #[arg(long, conflicts_with = "cloud")]
        local: bool,
    },

    /// Stream machine logs
    Logs(crate::commands::logs::LogsCmd),

    /// Copy files between host and machine
    Cp(crate::commands::cp::CpCmd),

    /// Branch a running, branchable machine into an independent child (CoW RAM + disks)
    #[command(name = "branch", visible_alias = "fork")]
    Branch(crate::commands::fork::ForkCmd),

    // --- maintenance / introspection --------------------------------------
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
            // lifecycle
            MachineSubcommand::Create(cmd) => cmd.run(),
            MachineSubcommand::Start(cmd) => cmd.run(),
            MachineSubcommand::Stop(cmd) => cmd.run(),
            MachineSubcommand::Rm(cmd) => cmd.run(),
            MachineSubcommand::Ls(cmd) => cmd.run(),
            MachineSubcommand::Status(cmd) => cmd.run(),
            MachineSubcommand::Exec(cmd) => cmd.run(),
            MachineSubcommand::Shell { name, cloud, local } => crate::commands::exec::ExecCmd {
                name,
                command: vec!["/bin/sh".to_string()],
                interactive: true,
                tty: true,
                stream: false,
                env: vec![],
                workdir: None,
                secret_env: vec![],
                secret_file: vec![],
                timeout: None,
                cloud,
                local,
            }
            .run(),
            MachineSubcommand::Logs(cmd) => cmd.run(),
            MachineSubcommand::Cp(cmd) => cmd.run(),
            MachineSubcommand::Branch(cmd) => cmd.run(),
            // maintenance
            MachineSubcommand::Images(cmd) => cmd.run(),
            MachineSubcommand::Prune(cmd) => cmd.run(),
            MachineSubcommand::Update(cmd) => cmd.run(),
            MachineSubcommand::Monitor(cmd) => cmd.run(),
            MachineSubcommand::DataDir(cmd) => cmd.run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: MachineSubcommand,
    }

    #[test]
    fn branch_is_primary_and_fork_remains_an_alias() {
        let branch = TestCli::parse_from([
            "smol",
            "branch",
            "--from",
            "source",
            "--name",
            "child",
            "--branchable",
        ]);
        let MachineSubcommand::Branch(branch) = branch.command else {
            panic!("expected branch command");
        };
        assert_eq!(branch.golden, "source");
        assert_eq!(branch.name, "child");
        assert!(branch.forkable);

        let fork = TestCli::parse_from([
            "smol",
            "fork",
            "--golden",
            "source",
            "--name",
            "child",
            "--forkable",
        ]);
        let MachineSubcommand::Branch(fork) = fork.command else {
            panic!("expected fork compatibility alias");
        };
        assert!(fork.forkable);
    }

    #[test]
    fn start_accepts_branchable_and_legacy_forkable() {
        for flag in ["--branchable", "--forkable"] {
            let parsed = TestCli::parse_from(["smol", "start", "--name", "source", flag]);
            let MachineSubcommand::Start(start) = parsed.command else {
                panic!("expected start command");
            };
            assert!(start.forkable);
        }
    }
}
