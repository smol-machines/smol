//! A persisted machine user applies to every local execution mode.
#![cfg(unix)]

use std::process::{Command, Output};

const NAME: &str = "exec-user-regression";

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_smol"))
            .args(args)
            .env("SMOLVM_DATA_DIR", self.0.path())
            .env("SMOLVM_CONFIG", self.0.path().join("settings.toml"))
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["machine", "stop", "--local", "--name", NAME]);
        let _ = self.run(&["machine", "rm", "--local", "--name", NAME, "--yes"]);
    }
}

#[test]
#[ignore = "boots an isolated image machine; requires a hypervisor, guest rootfs and registry access"]
fn configured_user_survives_all_exec_modes_and_temporary_overrides() {
    let fixture = Fixture(
        tempfile::Builder::new()
            .prefix("su-")
            .tempdir_in("/tmp")
            .unwrap(),
    );
    fixture.ok(&[
        "machine",
        "create",
        "--local",
        "--name",
        NAME,
        "--image",
        "alpine:3.21",
        "--user",
        "1001:1001",
        "--cpus",
        "1",
        "--mem",
        "512",
        "--net",
    ]);
    fixture.ok(&["machine", "start", "--local", "--name", NAME]);
    for mode in [None, Some("--stream"), Some("--interactive")] {
        for (user, expected) in [(None, "1001"), (Some("0"), "0"), (None, "1001")] {
            let mut args = vec!["machine", "exec", "--local", "--name", NAME];
            if let Some(mode) = mode {
                args.push(mode);
            }
            if let Some(user) = user {
                args.extend(["--user", user]);
            }
            args.extend(["--", "id", "-u"]);
            assert_eq!(
                fixture.ok(&args).trim(),
                expected,
                "mode={mode:?}, override={user:?}"
            );
        }
    }
    // The choice survives a boot too, rather than depending on an exec cache.
    fixture.ok(&["machine", "stop", "--local", "--name", NAME]);
    fixture.ok(&["machine", "start", "--local", "--name", NAME]);
    assert_eq!(
        fixture
            .ok(&["machine", "exec", "--local", "--name", NAME, "--", "id", "-u"])
            .trim(),
        "1001"
    );
}
