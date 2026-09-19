//! Walk every local operation the SDK exposes, and say which ones work.

use std::time::Instant;

use smolmachines::{
    configure_runtime_assets, BranchOptions, CheckpointOptions, ExecEvent, ExecOptions, Machine,
    Mount, Port, Result, RuntimeAssets,
};

/// A command that ran but failed is a failure: `exec` reports the guest's exit
/// code rather than erroring, so checking only Ok/Err hides a dead machine
/// behind an empty string.
fn ran(result: Result<smolmachines::ExecResult>) -> Result<String> {
    let result = result?;
    if result.success() {
        Ok(result.stdout_utf8().trim().to_string())
    } else {
        Err(smolmachines::Error::new(
            smolmachines::ErrorKind::CommandFailed,
            format!("exit {}: {}", result.exit_code, result.stderr_utf8().trim()),
        ))
    }
}

/// An operation this target is expected to refuse.
fn refused(outcome: Result<String>) -> Result<String> {
    match outcome {
        Err(e) if e.kind() == smolmachines::ErrorKind::NotSupported => {
            Ok("correctly refused".into())
        }
        other => other.map(|_| "unexpectedly succeeded".into()),
    }
}

fn step(name: &str, outcome: Result<String>) -> bool {
    match outcome {
        Ok(detail) => {
            println!("  PASS  {name:<22} {detail}");
            true
        }
        Err(error) => {
            println!("  FAIL  {name:<22} [{}] {}", error.code(), error.message());
            false
        }
    }
}

fn main() -> Result<()> {
    if let Some(assets) = RuntimeAssets::from_path_lookup() {
        configure_runtime_assets(assets)?;
    }
    let scratch = std::env::temp_dir().join("smolmachines-e2e");
    let staged = scratch.join("staged");
    std::fs::create_dir_all(&staged).expect("scratch");
    std::fs::write(staged.join("from-host.txt"), "host wrote this").expect("seed");

    let mut failures = 0;
    let mut check = |name: &str, outcome: Result<String>| {
        if !step(name, outcome) {
            failures += 1;
        }
    };

    let machine = Machine::builder("e2e-local")
        .image("alpine:latest")
        .network(true)
        .memory_mib(1024)
        .port(Port::new(18080, 80))
        .mount(Mount::new(staged.to_string_lossy().to_string(), "/staged").staged())
        .branchable(true)
        .create()?;
    check("create", Ok(machine.name().to_string()));

    let started = Instant::now();
    check(
        "start_branchable",
        machine
            .start_branchable()
            .map(|()| format!("{:?}", started.elapsed())),
    );
    check("state", Ok(machine.state().to_string()));
    check("ready", machine.ready().map(|r| r.to_string()));
    check(
        "wait_until_ready",
        machine.wait_until_ready().map(|()| "ok".into()),
    );
    check("pid", Ok(format!("{:?}", machine.pid())));

    check(
        "exec",
        machine
            .exec(["uname", "-m"])
            .map(|r| r.stdout_utf8().trim().to_string()),
    );
    check(
        "exec_with(env)",
        machine
            .exec_with(
                ["sh", "-c", "echo $MARKER"],
                ExecOptions::new().env("MARKER", "set"),
            )
            .map(|r| r.stdout_utf8().trim().to_string()),
    );

    // The local streaming path has its own producer thread; exercise it.
    let stream = machine.exec_stream(["sh", "-c", "echo one; echo two >&2"], ExecOptions::new())?;
    let events: Vec<_> = stream.collect();
    let exited = events.iter().any(|e| matches!(e, ExecEvent::Exit(0)));
    let saw_stdout = events
        .iter()
        .any(|e| matches!(e, ExecEvent::Stdout(b) if b.starts_with(b"one")));
    check(
        "exec_stream",
        if exited && saw_stdout {
            Ok(format!("{} events", events.len()))
        } else {
            Err(smolmachines::Error::new(
                smolmachines::ErrorKind::Other,
                format!("unexpected events: {events:?}"),
            ))
        },
    );

    check(
        "write_file",
        machine
            .write_file("/tmp/note", "written by the sdk")
            .map(|()| "ok".into()),
    );
    check(
        "read_file",
        machine
            .read_file("/tmp/note")
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    );
    check(
        "write_file_with_mode",
        machine
            .write_file_with_mode("/tmp/run.sh", "#!/bin/sh\necho scripted\n", 0o755)
            .and_then(|()| ran(machine.exec(["/tmp/run.sh"]))),
    );

    check(
        "staged mount",
        machine
            .exec(["cat", "/staged/from-host.txt"])
            .map(|r| r.stdout_utf8().trim().to_string()),
    );
    check(
        "sync",
        machine
            .exec(["sh", "-c", "echo guest wrote this > /staged/from-guest.txt"])
            .and_then(|_| machine.sync())
            .map(|()| {
                std::fs::read_to_string(staged.join("from-guest.txt"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|e| format!("host file missing: {e}"))
            }),
    );

    check(
        "guest_ports",
        machine.guest_ports().map(|p| format!("{p:?}")),
    );
    check("host_port", machine.host_port(80).map(|p| format!("{p:?}")));
    check(
        "endpoint",
        machine.endpoint(80, "/health").map(|e| e.http_url),
    );
    check("url", machine.url().map(|u| format!("{u:?}")));

    check(
        "pull_image (no local verb)",
        refused(machine.pull_image("alpine:3.20").map(|i| i.reference)),
    );
    check(
        "list_images",
        machine.list_images().map(|i| format!("{} images", i.len())),
    );

    // The engine refuses to branch or capture a machine carrying host mounts,
    // so those phases get a machine of their own rather than a false failure.
    let clean = Machine::builder("e2e-clean")
        .image("alpine:latest")
        .network(true)
        .memory_mib(1024)
        .branchable(true)
        .create()?;
    clean.start_branchable()?;
    clean.write_file("/workspace/note", "written by the sdk")?;

    let store = scratch.join("store");
    let artifact = scratch.join("cold.smolcheckpoint");
    // The engine refuses to overwrite an artifact, so the warm capture needs
    // its own path. The store is what makes it incremental, not the file.
    let warm_artifact = scratch.join("warm.smolcheckpoint");
    check(
        "checkpoint (cold)",
        clean
            .checkpoint_with(Some(&artifact), CheckpointOptions::new().store_dir(&store))
            .map(|c| {
                let r = c.local().expect("local capture");
                format!("{} MiB in {:?}", r.size_bytes >> 20, r.elapsed)
            }),
    );
    check(
        "checkpoint (warm)",
        clean
            .checkpoint_with(
                Some(&warm_artifact),
                CheckpointOptions::new().store_dir(&store),
            )
            .map(|c| {
                let r = c.local().expect("local capture");
                format!(
                    "{} MiB written, {} MiB reused",
                    r.size_bytes >> 20,
                    r.reused_bytes >> 20
                )
            }),
    );
    check(
        "checkpoints (cloud-only)",
        match clean.checkpoints() {
            Err(e) if e.kind() == smolmachines::ErrorKind::NotSupported => {
                Ok("correctly refused".into())
            }
            other => other.map(|_| "unexpectedly succeeded".into()),
        },
    );

    let branch_started = Instant::now();
    let branch = clean.branch("e2e-local-branch");
    check(
        "branch",
        branch.as_ref().map_err(Clone::clone).and_then(|b| {
            b.exec(["cat", "/workspace/note"]).map(|r| {
                format!(
                    "{:?}, child sees: {}",
                    branch_started.elapsed(),
                    r.stdout_utf8().trim()
                )
            })
        }),
    );

    let batch_started = Instant::now();
    let names: Vec<String> = (0..4).map(|i| format!("e2e-local-batch-{i}")).collect();
    let batch = clean.branch_batch(names, BranchOptions::new().parallel(4));
    check(
        "branch_batch",
        batch
            .as_ref()
            .map_err(Clone::clone)
            .map(|b| format!("{} branches in {:?}", b.len(), batch_started.elapsed())),
    );

    check(
        "usage (cloud-only)",
        match clean.usage() {
            Err(e) if e.kind() == smolmachines::ErrorKind::NotSupported => {
                Ok("correctly refused".into())
            }
            other => other.map(|_| "unexpectedly succeeded".into()),
        },
    );

    // Tear the children down before the parent.
    if let Ok(batch) = &batch {
        for child in batch {
            let _ = child.delete();
        }
    }
    if let Ok(branch) = &branch {
        let _ = branch.delete();
    }

    check("stop", machine.stop().map(|()| machine.state().to_string()));
    check(
        "start (restart)",
        machine.start().map(|()| machine.state().to_string()),
    );
    check(
        "exec after restart",
        machine
            .exec([
                "sh",
                "-c",
                "echo survived > /workspace/persisted; cat /workspace/persisted",
            ])
            .map(|r| r.stdout_utf8().trim().to_string()),
    );

    check(
        "connect",
        Machine::connect("e2e-local").map(|m| m.name().to_string()),
    );

    check(
        "restore_checkpoint",
        Machine::restore_checkpoint("e2e-local-restored", &artifact).and_then(|restored| {
            restored.start()?;
            let seen = restored.exec(["cat", "/workspace/note"])?;
            let detail = seen.stdout_utf8().trim().to_string();
            restored.delete()?;
            Ok(detail)
        }),
    );

    check("delete", machine.delete().map(|()| "ok".into()));
    check(
        "delete (branch source)",
        clean.delete().map(|()| "ok".into()),
    );

    std::fs::remove_dir_all(&scratch).ok();
    println!("\n{} failures", failures);
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}
