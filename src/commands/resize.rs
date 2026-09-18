//! Grow a running machine without restarting its workload.

use clap::{ArgGroup, Args};

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("resources").required(true).multiple(true).args(["cpus", "memory", "storage", "overlay"])))]
pub struct ResizeCmd {
    /// Machine to grow (default: "default")
    #[arg(short = 'n', long)]
    pub name: Option<String>,
    /// Total virtual CPUs, not the number to add
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..))]
    pub cpus: Option<u8>,
    /// Total guest RAM in MiB
    #[arg(long = "mem", visible_alias = "memory", value_parser = clap::value_parser!(u32).range(1..))]
    pub memory: Option<u32>,
    /// Total storage disk capacity in GiB
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub storage: Option<u64>,
    /// Total writable root overlay capacity in GiB
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub overlay: Option<u64>,
    /// Resolve only local machines
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
    /// Resolve only cloud machines
    #[arg(long)]
    pub cloud: bool,
}

impl ResizeCmd {
    pub fn run(self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};
        let (location, name) = resolve::route(
            self.name.as_deref(),
            Target::from_flags(self.local, self.cloud)?,
        )?;
        if location == Location::Cloud {
            anyhow::bail!("live resize requires cloud control-plane support; it is not available on this transport yet");
        }
        let record = smolvm::embedded::runtime()?.resize_machine(
            &name,
            smolvm::embedded::ResizeSpec {
                cpus: self.cpus,
                memory_mib: self.memory,
                storage_gib: self.storage,
                overlay_gib: self.overlay,
            },
        )?;
        println!(
            "Resized running machine '{name}' without rebooting: {} CPUs, {} MiB RAM",
            record.cpus, record.mem
        );
        if let Some(size) = self.storage {
            println!("Storage filesystem: {size} GiB");
        }
        if let Some(size) = self.overlay {
            println!("Overlay filesystem: {size} GiB");
        }
        Ok(())
    }
}
