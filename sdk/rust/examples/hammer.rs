use smolmachines::{ExecOptions, Machine, Result};
fn main() -> Result<()> {
    let m = Machine::builder("hammer")
        .image("alpine:latest")
        .network(true)
        .memory_mib(1024)
        .branchable(true)
        .create()?;
    m.start_branchable()?;
    for i in 1..=60 {
        // Alternate the call shapes the sweep uses, back to back.
        if let Err(e) = m.exec(["true"]) {
            println!(
                "  exec failed at {i}: {}",
                e.message().chars().take(70).collect::<String>()
            );
            break;
        }
        if i % 3 == 0 {
            if let Err(e) = m
                .exec_stream(["echo", "x"], ExecOptions::new())
                .map(|s| s.count())
            {
                println!(
                    "  stream failed at {i}: {}",
                    e.message().chars().take(70).collect::<String>()
                );
                break;
            }
        }
        if i % 5 == 0 {
            if let Err(e) = m.write_file("/tmp/h", "x") {
                println!(
                    "  write failed at {i}: {}",
                    e.message().chars().take(70).collect::<String>()
                );
                break;
            }
        }
        if i % 10 == 0 {
            println!("  {i:3} calls, state={}", m.state());
        }
    }
    println!("  final state={}", m.state());
    let _ = m.delete();
    Ok(())
}
