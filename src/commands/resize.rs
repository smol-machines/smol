//! smol machine resize — grow a running machine without rebooting it.

use clap::Args;
use smolvm::config::RecordState;
use smolvm::db::SmolvmDb;

/// Add CPUs, RAM or disk to a running machine without rebooting it.
///
/// Sizes are totals, not increments. RAM and disks only grow; CPUs can also
/// shrink on Linux x86_64. Several resources in one command are checked
/// together before anything changes, then applied RAM first, CPUs, then
/// disks. Use `smol machine update` to change a stopped machine.
///
/// Examples:
///   smol machine resize -n agent --cpus 4
///   smol machine resize -n agent --mem 4096
///   smol machine resize -n agent --cpus 4 --mem 4096 --storage 50
#[derive(Args, Debug)]
#[command(group(
    clap::ArgGroup::new("resize-target")
        .required(true)
        .args(["cpus", "mem", "storage", "overlay"])
        .multiple(true)
))]
pub struct ResizeCmd {
    /// Machine to resize (default: "default")
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Total vCPUs
    #[arg(long, value_name = "COUNT", value_parser = clap::value_parser!(u8).range(1..))]
    pub cpus: Option<u8>,

    /// Total RAM in MiB (grow only)
    #[arg(long, value_name = "MiB", value_parser = clap::value_parser!(u32).range(1..))]
    pub mem: Option<u32>,

    /// Storage disk size in GiB (grow only)
    #[arg(long, value_name = "GiB", value_parser = clap::value_parser!(u64).range(1..))]
    pub storage: Option<u64>,

    /// Overlay disk size in GiB (grow only)
    #[arg(long, value_name = "GiB", value_parser = clap::value_parser!(u64).range(1..))]
    pub overlay: Option<u64>,

    /// Resize a cloud machine. Not available yet; live resize is local-only.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl ResizeCmd {
    pub fn run(self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, name) = resolve::route(self.name.as_deref(), target)?;
        if location == Location::Cloud {
            anyhow::bail!("live resize is only available for local machines");
        }

        let db = SmolvmDb::open()?;
        let record = db
            .get_vm(&name)?
            .ok_or_else(|| smolvm::Error::vm_not_found(&name))?;
        if record.actual_state() != RecordState::Running {
            anyhow::bail!(
                "machine '{name}' is not running; use `smol machine update` to change a stopped machine"
            );
        }

        smolvm::agent::live_resize::check_targets(
            &db,
            &name,
            self.cpus,
            self.mem,
            self.storage,
            self.overlay,
        )?;
        // RAM first: its host headroom check is the likeliest refusal, and it
        // refuses before anything changes.
        let mut resized = record;
        if let Some(mem) = self.mem {
            resized = smolvm::agent::live_resize::grow_memory(&db, &name, mem)?;
            println!("RAM: {} MiB", resized.mem);
        }
        if let Some(cpus) = self.cpus {
            resized = smolvm::agent::live_resize::grow_cpus(&db, &name, cpus)?;
            println!("CPUs: {}", resized.cpus);
        }
        if self.storage.is_some() || self.overlay.is_some() {
            resized =
                smolvm::agent::live_resize::grow_disks(&db, &name, self.storage, self.overlay)?;
            if let Some(size) = self.storage {
                println!("Storage: {size} GiB");
            }
            if let Some(size) = self.overlay {
                println!("Overlay: {size} GiB");
            }
        }
        println!(
            "Resized running machine '{name}' without rebooting: {} CPUs, {} MiB RAM",
            resized.cpus, resized.mem
        );
        Ok(())
    }
}
