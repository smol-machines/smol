use smolmachines::{ExecEvent, ExecOptions, Machine, Mount, Port, Result};
fn main() -> Result<()> {
    let m = Machine::builder("seqdiag")
        .image("alpine:latest")
        .network(true)
        .memory_mib(1024)
        .port(Port::new(18082, 80))
        .mount(Mount::new("/tmp/stg4", "/staged").staged())
        .branchable(true)
        .create()?;
    let s = |l: &str, m: &Machine| println!("  {l:<24} {}", m.state());
    s("create", &m);
    m.start_branchable()?;
    s("start_branchable", &m);
    let _ = m.state();
    s("state", &m);
    let _ = m.ready()?;
    s("ready", &m);
    m.wait_until_ready()?;
    s("wait_until_ready", &m);
    let _ = m.pid();
    s("pid", &m);
    let _ = m.exec(["uname", "-m"])?;
    s("exec", &m);
    let _ = m.exec_with(
        ["sh", "-c", "echo $MARKER"],
        ExecOptions::new().env("MARKER", "set"),
    )?;
    s("exec_with(env)", &m);
    let ev: Vec<ExecEvent> = m
        .exec_stream(["sh", "-c", "echo one; echo two >&2"], ExecOptions::new())?
        .collect();
    println!("  {:<24} {} events", "exec_stream", ev.len());
    s("after exec_stream", &m);
    let _ = m.delete();
    Ok(())
}
