//! Drive an agent session end to end with the model-free `command` harness:
//! two turns, rewind, branch, pause/resume, delete.
//!
//!     cargo run --example agent_session
use smolmachines::agent::{Harness, Session, SessionOptions};

fn main() -> smolmachines::Result<()> {
    let harness = Harness::Command {
        image: "alpine:3.20".into(),
        program: vec!["sh".into(), "-c".into()],
    };
    let mut opts = SessionOptions::new("demo-agent", harness);
    opts.memory_mib = 512;
    let started = std::time::Instant::now();
    let mut session = Session::start(opts)?;
    println!(
        "started in {:?} on machine {}",
        started.elapsed(),
        session.record().machine
    );

    let mut print = |e: &smolmachines::agent::AgentEvent| println!("  event: {e:?}");
    let t0 = session.send(
        "echo one > /workspace/story.txt; cat /workspace/story.txt",
        &mut print,
    )?;
    println!(
        "turn 0 -> {:?} checkpoint={:?}",
        t0.result,
        t0.checkpoint.is_some()
    );
    let t1 = session.send(
        "echo two >> /workspace/story.txt; cat /workspace/story.txt",
        &mut print,
    )?;
    println!("turn 1 -> {:?}", t1.result);

    let branch = session.branch(1, "demo-agent-branch")?;
    let branched = branch
        .machine()?
        .exec(["cat", "/workspace/story.txt"])?
        .stdout_utf8()
        .to_string();
    println!("branch at turn 1 sees: {:?}", branched.trim());

    let t = std::time::Instant::now();
    session.rewind(0)?;
    println!(
        "rewound to turn 0 in {:?}; machine now {}",
        t.elapsed(),
        session.record().machine
    );
    let after = session
        .machine()?
        .exec(["cat", "/workspace/story.txt"])?
        .stdout_utf8()
        .to_string();
    println!("after rewind the story is: {:?}", after.trim());

    let t2 = session.send(
        "echo three >> /workspace/story.txt; cat /workspace/story.txt",
        &mut print,
    )?;
    println!("new turn 1 after rewind -> {:?}", t2.result);

    session.pause()?;
    println!("paused; state {:?}", session.machine()?.state());
    session.resume()?;
    println!("resumed; state {:?}", session.machine()?.state());

    branch.delete()?;
    session.delete()?;
    println!("deleted");
    Ok(())
}
