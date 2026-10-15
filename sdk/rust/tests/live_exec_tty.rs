//! Live checks against a real engine. Run with:
//!   SMOLMACHINES_LIVE_IMAGE=alpine:3.20 cargo test --test live_exec_tty -- --ignored --nocapture
use std::io::Write;
use std::time::Duration;

use smolmachines::{ExecEvent, ExecOptions, Machine, PackOptions};

fn image() -> Option<String> {
    std::env::var("SMOLMACHINES_LIVE_IMAGE").ok()
}

#[test]
#[ignore]
fn tty_stdin_and_pack_export_work_on_a_real_machine() {
    let Some(image) = image() else {
        return;
    };
    let name = format!("sdk-live-{}", std::process::id());
    let machine = Machine::builder(&name)
        .image(image)
        .network(true)
        .cpus(1)
        .memory_mib(512)
        .allow_host_pattern("api.example.com")
        .create()
        .expect("create");
    machine.start().expect("start");
    machine.wait_until_ready().expect("ready");

    // A pseudo-terminal: `tty` succeeds and names a pts.
    let out = machine
        .exec_stream(
            ["sh", "-c", "tty; echo tty-ok"],
            ExecOptions::new().tty(true),
        )
        .expect("exec tty")
        .collect_result();
    assert_eq!(out.exit_code, 0, "{}", out.stderr_utf8());
    let text = out.stdout_utf8();
    assert!(
        text.contains("/dev/pts/") || text.contains("/dev/console"),
        "{text}"
    );
    assert!(text.contains("tty-ok"), "{text}");

    // Piped stdin: cat echoes what we write.
    let mut stream = machine
        .exec_stream(
            ["cat"],
            ExecOptions::new()
                .stdin(true)
                .timeout(Duration::from_secs(30)),
        )
        .expect("exec stdin");
    let mut stdin = stream.stdin().expect("stdin");
    stdin.write_all(b"hello through stdin\n").unwrap();
    drop(stdin);
    let mut stdout = Vec::new();
    for event in stream {
        if let ExecEvent::Stdout(chunk) = event {
            stdout.extend(chunk);
        }
    }
    assert_eq!(String::from_utf8_lossy(&stdout), "hello through stdin\n");

    machine.stop().expect("stop");
    let out = std::env::temp_dir().join(format!("{name}.smolmachine"));
    let written = machine
        .export_pack(&out, PackOptions::new())
        .expect("export pack");
    assert!(written.is_file(), "{}", written.display());
    let _ = std::fs::remove_file(&written);
    machine.delete().expect("delete");
}
