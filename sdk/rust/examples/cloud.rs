//! Run a machine on smol cloud instead of this process.
//!
//! The same `Machine` API as the local examples; only the `ConnectOptions`
//! differ. Credentials come from `SMOL_CLOUD_TOKEN`, or the session that
//! `smol auth login` left on disk.
//!
//! ```sh
//! cargo run --example cloud
//! ```

use smolmachines::{list_cloud_machines, ConnectOptions, Result};

fn main() -> Result<()> {
    let connect = ConnectOptions::cloud();

    let machines = list_cloud_machines(&connect)?;
    println!("{} machines in this account", machines.len());

    // Creating on the cloud also starts the machine and waits for its agent,
    // so it is ready to work by the time this returns.
    let machine = smolmachines::Machine::builder("sdk-cloud-example")
        .image("alpine:latest")
        // Without this an idle machine runs, and bills, until something stops it.
        .auto_stop_seconds(300)
        .create_with(&connect)?;
    println!("created {} ({})", machine.name(), machine.id());

    let result = machine.exec(["sh", "-c", "uname -sm; cat /etc/alpine-release"])?;
    print!("{}", result.stdout_utf8());

    machine.write_file("/tmp/note", "written from Rust")?;
    let read_back = machine.read_file("/tmp/note")?;
    println!("read back: {}", String::from_utf8_lossy(&read_back));

    let usage = machine.usage()?;
    println!(
        "{:.0}s uptime, {} µ$ so far",
        usage.usage.total_uptime_seconds, usage.cost.total_micros
    );

    machine.delete()?;
    println!("deleted");
    Ok(())
}
