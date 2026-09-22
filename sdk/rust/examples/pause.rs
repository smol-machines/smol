//! Pause a local machine, then resume its running execution.
use smolmachines::{Machine, Result};

fn main() -> Result<()> {
    let machine = Machine::builder(format!("pause-example-{}", std::process::id()))
        .cpus(1)
        .memory_mib(512)
        .storage_gib(2)
        .overlay_gib(1)
        .branchable(true)
        .persistent(true)
        .create()?;
    let outcome = (|| -> Result<()> {
        machine.start_branchable()?;
        let before = machine.exec([
            "sh",
            "-c",
            "echo remembered >/dev/shm/note; cat /proc/sys/kernel/random/boot_id",
        ])?;
        assert!(before.success());
        machine.pause()?;
        assert!(machine.start().is_err());
        machine.resume()?;
        let after = machine.exec([
            "sh",
            "-c",
            "test \"$(cat /dev/shm/note)\" = remembered && cat /proc/sys/kernel/random/boot_id",
        ])?;
        assert!(after.success());
        assert_eq!(before.stdout_utf8(), after.stdout_utf8());
        println!("Resumed the same guest with its RAM-only data intact.");
        Ok(())
    })();
    let cleanup = machine.delete();
    outcome?;
    cleanup
}
