//! Boot a machine, run a command in it, tear it down.
//!
//! ```sh
//! cargo run --example hello
//! ```

use smolmachines::{configure_runtime_assets, Machine, Result, RuntimeAssets};

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

    let machine = Machine::builder("sdk-hello")
        .image("alpine:latest")
        .cpus(2)
        .memory_mib(1024)
        .network(true)
        .create()?;

    machine.start()?;
    println!("machine is {} (pid {:?})", machine.state(), machine.pid());

    let result = machine.exec(["sh", "-c", "uname -sm; cat /etc/alpine-release"])?;
    print!("{}", result.stdout_utf8());
    if !result.success() {
        eprint!("{}", result.stderr_utf8());
    }

    machine.delete()?;
    Ok(())
}
