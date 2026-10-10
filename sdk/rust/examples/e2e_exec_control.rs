//! Per-command user, killing a streamed command, and concurrent commands, on a
//! bare VM and an image machine. Exits non-zero on any failure.
//!
//! Liveness is checked with a heartbeat file rather than `ps`: on an image
//! machine each command is its own container, so one exec cannot see another's
//! processes.

use std::thread;
use std::time::{Duration, Instant};

use smolmachines::{configure_runtime_assets, ExecEvent, ExecOptions, Machine, RuntimeAssets};

struct Checks(usize);

impl Checks {
    fn check(&mut self, label: &str, ok: bool, detail: impl AsRef<str>) {
        if !ok {
            self.0 += 1;
        }
        let status = if ok { "PASS" } else { "FAIL" };
        println!("  {status}  {label}  ({})", detail.as_ref());
    }
}

fn heartbeat(file: &str) -> Vec<String> {
    let script = format!(
        "mkdir -p /workspace && i=0; while true; do i=$((i+1)); echo $i > /workspace/{file}; sleep 0.2; done"
    );
    vec!["sh".into(), "-c".into(), script]
}

/// Whether the heartbeat file is still changing.
fn beating(machine: &Machine, file: &str) -> bool {
    let read = || {
        machine
            .exec(["cat", &format!("/workspace/{file}")])
            .map(|r| r.stdout_utf8().trim().to_string())
            .unwrap_or_default()
    };
    let first = read();
    thread::sleep(Duration::from_millis(900));
    !first.is_empty() && read() != first
}

fn suite(checks: &mut Checks, label: &str, machine: &Machine) -> smolmachines::Result<()> {
    println!("\n== {label}");

    // A long stream does not hold up other commands.
    let long = machine.exec_stream(["sh", "-c", "sleep 5; echo done-long"], ExecOptions::new())?;
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    let during = machine.exec(["echo", "hi"])?;
    let took = started.elapsed();
    checks.check(
        "exec stays fast while a stream runs",
        during.stdout_utf8().trim() == "hi" && took < Duration::from_secs(2),
        format!("{took:?}"),
    );
    let long = long.collect_result();
    checks.check(
        "the long stream still completes",
        long.stdout_utf8().trim() == "done-long" && long.exit_code == 0,
        long.stdout_utf8().trim(),
    );

    // Killing a stream from another thread kills the command in the machine.
    let mut stream = machine.exec_stream(heartbeat("hb-kill"), ExecOptions::new())?;
    thread::sleep(Duration::from_millis(1000));
    checks.check(
        "streamed command runs before the kill",
        beating(machine, "hb-kill"),
        "",
    );
    let handle = stream.kill_handle();
    let killer = thread::spawn(move || handle.kill());
    let started = Instant::now();
    let rest = stream.next();
    killer.join().expect("kill thread");
    checks.check(
        "a killed stream ends",
        rest.is_none(),
        format!("{rest:?} after {:?}", started.elapsed()),
    );
    thread::sleep(Duration::from_millis(300));
    checks.check(
        "the killed command is gone",
        !beating(machine, "hb-kill"),
        "",
    );

    // Dropping without a kill leaves the command running, as documented.
    let dropped = machine.exec_stream(heartbeat("hb-drop"), ExecOptions::new())?;
    drop(dropped);
    thread::sleep(Duration::from_millis(1000));
    checks.check(
        "a dropped stream's command keeps running",
        beating(machine, "hb-drop"),
        "",
    );

    // A timeout under a second is honored rather than rounded away.
    let started = Instant::now();
    let timed = machine.exec_with(
        ["sleep", "10"],
        ExecOptions::new().timeout(Duration::from_millis(1500)),
    );
    let took = started.elapsed();
    checks.check(
        "a 1.5 s timeout ends the command",
        took < Duration::from_secs(6),
        format!("{took:?}, {:?}", timed.map(|r| r.exit_code)),
    );
    Ok(())
}

fn main() -> smolmachines::Result<()> {
    if let Some(assets) = RuntimeAssets::from_path_lookup() {
        configure_runtime_assets(assets)?;
    }
    let pid = std::process::id();
    let mut checks = Checks(0);
    let available = smolmachines::local_availability();
    checks.check(
        "this host reports it can run local machines",
        available.is_ok(),
        format!("{available:?}"),
    );

    let bare = Machine::builder(format!("rs-bare-{pid}")).create()?;
    let outcome = (|| {
        bare.start()?;
        suite(&mut checks, "bare VM", &bare)?;
        // The CLI refuses before running anything; the SDK reports a CLI
        // failure as a non-zero exit carrying the CLI's message.
        let refused = bare.exec_with(["id", "-u"], ExecOptions::new().user("nobody"));
        let detail = match &refused {
            Ok(r) => format!("exit {}: {}", r.exit_code, r.stderr_utf8().trim()),
            Err(e) => e.to_string(),
        };
        checks.check(
            "a bare VM refuses a per-command user",
            detail.contains("image machine") && !detail.starts_with("exit 0"),
            &detail,
        );
        Ok::<_, smolmachines::Error>(())
    })();
    let _ = bare.delete();
    outcome?;

    let image = Machine::builder(format!("rs-img-{pid}"))
        .image("public.ecr.aws/docker/library/alpine:3.20")
        .network(true)
        .create()?;
    let outcome = (|| {
        image.start()?;
        let as_user = image.exec_with(["id", "-u"], ExecOptions::new().user("nobody"))?;
        checks.check(
            "exec runs as the requested user",
            as_user.stdout_utf8().trim() == "65534",
            as_user.stdout_utf8().trim(),
        );
        let as_root = image.exec(["id", "-u"])?;
        checks.check(
            "exec without a user still runs as root",
            as_root.stdout_utf8().trim() == "0",
            as_root.stdout_utf8().trim(),
        );
        let streamed: Vec<u8> = image
            .exec_stream(["id", "-u"], ExecOptions::new().user("nobody"))?
            .filter_map(|e| match e {
                ExecEvent::Stdout(b) => Some(b),
                _ => None,
            })
            .flatten()
            .collect();
        let streamed = String::from_utf8_lossy(&streamed).trim().to_string();
        checks.check(
            "exec_stream runs as the requested user",
            streamed == "65534",
            streamed,
        );
        let unknown = image.exec_with(["id"], ExecOptions::new().user("no-such-user"));
        checks.check(
            "an unknown user fails instead of running as root",
            !matches!(&unknown, Ok(r) if r.success()),
            format!("{:?}", unknown.map(|r| r.exit_code).map_err(|e| e.kind())),
        );
        suite(&mut checks, "image machine (alpine)", &image)?;
        Ok::<_, smolmachines::Error>(())
    })();
    let _ = image.delete();
    outcome?;

    println!(
        "\n{}",
        if checks.0 == 0 {
            "ALL PASSED".to_string()
        } else {
            format!("{} FAILED", checks.0)
        }
    );
    std::process::exit(if checks.0 == 0 { 0 } else { 1 });
}
