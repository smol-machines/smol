//! Capture a cloud machine and bring it back.
//!
//! The control plane stores the capture, so nothing touches your disk: the
//! capture is addressed by id, and restoring creates a new machine from it.
//!
//! ```sh
//! cargo run --example cloud_checkpoint
//! ```

use smolmachines::{ConnectOptions, Machine, Result};

fn main() -> Result<()> {
    let connect = ConnectOptions::cloud();

    let machine = Machine::builder("sdk-cloud-checkpoint")
        .image("alpine:latest")
        .branchable(true)
        .auto_stop_seconds(300)
        .create_with(&connect)?;
    machine.exec(["sh", "-c", "echo remembered > /tmp/note"])?;
    println!("created {}", machine.id());

    let captured = machine.checkpoint(None)?;
    let stored = captured
        .cloud()
        .expect("a cloud machine captures to the cloud");
    println!(
        "captured {} ({} MiB, {}, status {})",
        stored.id,
        stored.size_bytes / (1 << 20),
        stored.arch,
        stored.status
    );

    let restored = Machine::restore_cloud_checkpoint("sdk-cloud-restored", &stored.id, &connect)?;
    let note = restored.exec(["cat", "/tmp/note"])?;
    println!(
        "restored {} remembers: {}",
        restored.id(),
        note.stdout_utf8().trim()
    );

    let bill = restored.delete_with_usage()?;
    println!("restored machine cost {} µ$", bill.cost.total_micros);
    machine.delete()?;
    Ok(())
}
