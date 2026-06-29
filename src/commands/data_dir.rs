//! smol data-dir — print the on-disk data directory path for a machine.

use clap::Args;
use smolvm::db::SmolvmDb;

#[derive(Args, Debug)]
pub struct DataDirCmd {
    /// Machine name
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,
}

impl DataDirCmd {
    pub fn run(self) -> anyhow::Result<()> {
        // Error for a machine that was never created, rather than printing a
        // computed path for a bogus name (consistent with status/start/rm).
        let db = SmolvmDb::open()?;
        if db.get_vm(&self.name)?.is_none() {
            anyhow::bail!("machine '{}' not found", self.name);
        }
        println!("{}", smolvm::agent::vm_data_dir(&self.name).display());
        Ok(())
    }
}
