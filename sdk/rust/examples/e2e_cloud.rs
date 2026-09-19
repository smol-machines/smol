//! Walk every cloud operation the SDK exposes, and say which ones work.

use std::time::Instant;

use smolmachines::{
    list_cloud_machines, BranchOptions, ConnectOptions, ExecEvent, ExecOptions, Machine, Result,
};

fn step(name: &str, outcome: Result<String>) -> bool {
    match outcome {
        Ok(detail) => {
            println!("  PASS  {name:<24} {detail}");
            true
        }
        Err(error) => {
            println!("  FAIL  {name:<24} [{}] {}", error.code(), error.message());
            false
        }
    }
}

fn main() -> Result<()> {
    let connect = ConnectOptions::cloud();
    let mut failures = 0;
    let mut check = |name: &str, outcome: Result<String>| {
        if !step(name, outcome) {
            failures += 1;
        }
    };

    check(
        "list_cloud_machines",
        list_cloud_machines(&connect).map(|m| format!("{} machines", m.len())),
    );

    let created = Instant::now();
    let machine = Machine::builder("e2e-cloud")
        .image("alpine:latest")
        .arch("arm64")
        .branchable(true)
        .auto_stop_seconds(300)
        .create_with(&connect)?;
    check("create + ready", Ok(format!("{:?}", created.elapsed())));
    check("id", Ok(machine.id().to_string()));
    check("state", Ok(machine.state().to_string()));
    check("ready", machine.ready().map(|r| r.to_string()));
    check("pid (none on cloud)", Ok(format!("{:?}", machine.pid())));

    check(
        "exec",
        machine
            .exec(["uname", "-m"])
            .map(|r| r.stdout_utf8().trim().to_string()),
    );
    check(
        "exec_with(timeout)",
        machine
            .exec_with(
                ["sh", "-c", "echo $MARKER"],
                ExecOptions::new()
                    .env("MARKER", "set")
                    .timeout(std::time::Duration::from_secs(30)),
            )
            .map(|r| r.stdout_utf8().trim().to_string()),
    );

    // The real server's SSE stream, not the mock's.
    let events: Vec<_> = machine
        .exec_stream(["sh", "-c", "echo one; echo two >&2"], ExecOptions::new())?
        .collect();
    let exited = events.iter().any(|e| matches!(e, ExecEvent::Exit(0)));
    check(
        "exec_stream (SSE)",
        if exited {
            Ok(format!("{} events", events.len()))
        } else {
            Err(smolmachines::Error::new(
                smolmachines::ErrorKind::Other,
                format!("no exit event: {events:?}"),
            ))
        },
    );

    check(
        "write_file",
        machine
            .write_file("/workspace/note", "written by the sdk")
            .map(|()| "ok".into()),
    );
    check(
        "read_file",
        machine
            .read_file("/workspace/note")
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    );
    check(
        "endpoint",
        machine.endpoint(8080, "/health").map(|e| e.http_url),
    );
    check("url", machine.url().map(|u| format!("{u:?}")));
    check(
        "guest_ports",
        machine.guest_ports().map(|p| format!("{p:?}")),
    );

    // This machine publishes no port, and the control plane refuses to hand out
    // a link that could not be reached. That refusal is the check: it proves
    // the call reaches the server and the error comes back intact. Sharing a
    // machine that does serve a port is not covered here.
    check(
        "share (needs a port)",
        match machine.share() {
            Err(e) if e.message().contains("publishes no ports") => Ok("correctly refused".into()),
            Ok(s) => Ok(format!("token {}…", &s.token[..s.token.len().min(8)])),
            Err(e) => Err(e),
        },
    );
    check("unshare", machine.unshare().map(|()| "ok".into()));

    let captured = machine.checkpoint(None);
    check(
        "checkpoint",
        captured.as_ref().map_err(Clone::clone).map(|c| {
            let s = c.cloud().expect("cloud capture");
            format!("{} ({} MiB, {})", s.id, s.size_bytes >> 20, s.arch)
        }),
    );
    check(
        "checkpoints (list)",
        machine.checkpoints().map(|c| format!("{} stored", c.len())),
    );

    if let Ok(captured) = &captured {
        let id = captured.cloud().expect("cloud capture").id.clone();
        check(
            "restore_cloud_checkpoint",
            Machine::restore_cloud_checkpoint("e2e-cloud-restored", &id, &connect).and_then(
                |restored| {
                    let seen = restored.exec(["cat", "/workspace/note"])?;
                    let detail = seen.stdout_utf8().trim().to_string();
                    restored.delete()?;
                    Ok(detail)
                },
            ),
        );
    }

    let branch_started = Instant::now();
    let branch = machine.branch("e2e-cloud-branch");
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
    if let Ok(branch) = &branch {
        let _ = branch.delete();
    }

    let batch_started = Instant::now();
    let names: Vec<String> = (0..3).map(|i| format!("e2e-cloud-batch-{i}")).collect();
    let batch = machine.branch_batch(names, BranchOptions::new().parallel(3));
    check(
        "branch_batch",
        batch
            .as_ref()
            .map_err(Clone::clone)
            .map(|b| format!("{} branches in {:?}", b.len(), batch_started.elapsed())),
    );
    if let Ok(batch) = &batch {
        for child in batch {
            let _ = child.delete();
        }
    }

    check(
        "connect_with",
        Machine::connect_with(machine.id(), &connect).map(|m| m.id().to_string()),
    );
    check("stop", machine.stop().map(|()| "ok".into()));
    check(
        "start (restart)",
        machine.start().map(|()| machine.state().to_string()),
    );
    check(
        "sync (local-only)",
        match machine.sync() {
            Err(e) if e.kind() == smolmachines::ErrorKind::NotSupported => {
                Ok("correctly refused".into())
            }
            other => other.map(|()| "unexpectedly succeeded".into()),
        },
    );
    check(
        "delete_with_usage",
        machine
            .delete_with_usage()
            .map(|u| format!("{} µ$", u.cost.total_micros)),
    );

    println!("\n{} failures", failures);
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}
