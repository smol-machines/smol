//! smol machine sync — copy staged mount contents back to the host.

use clap::Args;

#[derive(Args, Debug)]
pub struct SyncCmd {
    /// Machine to synchronize (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Force a local machine. Staged host mounts are local-only.
    #[arg(long)]
    pub local: bool,

    /// Reject explicitly: cloud machines cannot carry host-directory mounts.
    #[arg(long, conflicts_with = "local")]
    pub cloud: bool,
}

impl SyncCmd {
    pub fn run(self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, name) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            anyhow::bail!("staged mounts are local-only; cloud machines have no host directories to synchronize");
        }

        let db = smolvm::db::SmolvmDb::open()?;
        let record = db
            .get_vm(&name)?
            .ok_or_else(|| smolvm::Error::vm_not_found(&name))?;
        if record.staged_mounts.is_empty() {
            println!("Machine '{name}' has no staged mounts");
            return Ok(());
        }

        let _source_lock = smolvm::agent::fork::lock_fork_source(&name)?;
        let manager = smolvm::agent::AgentManager::for_vm(&name)?;
        if manager.try_connect_existing().is_none() {
            anyhow::bail!(
                "machine '{name}' is not running; start it before synchronizing staged mounts"
            );
        }
        let mut client = smolvm::agent::AgentClient::connect_with_retry(manager.vsock_socket())?;
        // A failed sync must leave the persistent machine available for retry.
        manager.detach();
        smolvm::staged_mount::sync_staged_mounts(&record, &mut client)?;
        println!("Synchronized staged mounts for '{name}'");
        Ok(())
    }
}
