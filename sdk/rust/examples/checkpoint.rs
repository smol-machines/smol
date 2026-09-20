//! Capture a running machine twice into one store, then restore it.
//!
//! The first capture writes everything. The second reuses every chunk that did
//! not change, which is what makes a warm capture cheap enough to take often.
//!
//! ```sh
//! cargo run --example checkpoint
//! ```

use smolmachines::{configure_runtime_assets, CheckpointOptions, Machine, Result, RuntimeAssets};

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

    let workdir = std::env::temp_dir().join("smolmachines-checkpoint-example");
    // The engine refuses to overwrite an artifact, so start from a clean slate
    // rather than failing on a second run.
    std::fs::remove_dir_all(&workdir).ok();
    std::fs::create_dir_all(&workdir).expect("create the example working directory");
    let store = workdir.join("store");

    let machine = Machine::builder("sdk-checkpoint")
        .image("alpine:latest")
        // The image is pulled from inside the guest, which needs networking.
        .network(true)
        .memory_mib(1024)
        .branchable(true)
        .create()?;
    machine.start_branchable()?;
    machine.exec(["sh", "-c", "echo first > /tmp/note"])?;

    for (round, note) in [("cold", "first"), ("warm", "second")] {
        machine.exec(["sh", "-c", &format!("echo {note} > /tmp/note")])?;
        let output = workdir.join(format!("{round}.smolcheckpoint"));
        let captured =
            machine.checkpoint_with(Some(&output), CheckpointOptions::new().store_dir(&store))?;
        // A local capture reports what it cost; a cloud one reports where it
        // is stored. This machine is local, so the local arm is the live one.
        let result = captured.local().expect("a local machine captures locally");
        println!(
            "{round}: {:?} wall, {:?} paused, {} MiB written, {} MiB reused",
            result.elapsed,
            result.source_pause,
            result.size_bytes / (1 << 20),
            result.reused_bytes / (1 << 20),
        );
    }

    let restored = Machine::restore_checkpoint(
        "sdk-checkpoint-restored",
        workdir.join("warm.smolcheckpoint"),
    )?;
    restored.start()?;
    let note = restored.exec(["cat", "/tmp/note"])?;
    println!("restored machine remembers: {}", note.stdout_utf8().trim());

    restored.delete()?;
    machine.delete()?;
    Ok(())
}
