//! Branch one warm machine many times and ask each branch who it is.
//!
//! The source pays for the boot once. Each branch starts from the source's
//! exact live state, sharing its RAM and disks copy-on-write, so a branch
//! costs a fraction of a boot.
//!
//! ```sh
//! cargo run --example fanout
//! ```

use smolmachines::{configure_runtime_assets, BranchOptions, Machine, Result, RuntimeAssets};

const BRANCHES: usize = 8;

/// Point the engine at an installed smolvm, so the boot helper is a binary that
/// knows how to be a VM rather than this example.
fn use_installed_engine() -> Result<()> {
    match RuntimeAssets::from_path_lookup() {
        Some(assets) => configure_runtime_assets(assets),
        None => Ok(()),
    }
}

fn main() -> Result<()> {
    use_installed_engine()?;

    let source = Machine::builder("sdk-fanout-source")
        .image("alpine:latest")
        // The image is pulled from inside the guest, which needs networking.
        .network(true)
        .memory_mib(512)
        .branchable(true)
        .create()?;

    // A branch source needs memfd-backed guest RAM, which only this start gives.
    source.start_branchable()?;
    source.exec(["sh", "-c", "echo shared-state > /tmp/source-marker"])?;

    let names: Vec<String> = (0..BRANCHES).map(|i| format!("sdk-fanout-{i}")).collect();
    let started = std::time::Instant::now();
    let branches = source.branch_batch(names, BranchOptions::new().parallel(4))?;
    println!("{} branches in {:?}", branches.len(), started.elapsed());

    for branch in &branches {
        // Every branch inherits the marker, because it inherited the RAM.
        let result = branch.exec(["cat", "/tmp/source-marker"])?;
        println!("{}: {}", branch.name(), result.stdout_utf8().trim());
    }

    for branch in &branches {
        branch.delete()?;
    }
    source.delete()?;
    Ok(())
}
