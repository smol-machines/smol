//! Standard SSH clients must keep the selected environment across cold boots.
#![cfg(unix)]

use std::{
    io::{Read, Seek, Write},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const NAME: &str = "ssh-cli-owned";

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_smol"));
        command
            .args(args)
            .env("SMOLVM_DATA_DIR", self.0.path())
            .env("SMOLVM_CONFIG", self.0.path().join("settings.toml"))
            .env("DOCKER_CONFIG", self.0.path().join("docker"))
            .env_remove("SMOLVM_BOOT_BINARY");
        command
    }

    fn ok(&self, args: &[&str]) -> Output {
        checked(&mut self.command(args))
    }

    fn db(&self) -> smolvm::Result<smolvm::db::SmolvmDb> {
        let base = if cfg!(target_os = "macos") {
            "Library/Application Support"
        } else {
            ".local/share"
        };
        smolvm::db::SmolvmDb::open_at(&self.0.path().join(base).join("smolvm/server/smolvm.db"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ =
            output(&mut self.command(&["_engine", "machine", "delete", "--name", NAME, "--force"]));
    }
}

fn output(command: &mut Command) -> Output {
    use std::os::unix::process::CommandExt;
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            // This group belongs to this invocation; committed VMs detach.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            panic!("owned SSH test command timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    stdout.rewind().unwrap();
    stderr.rewind().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    stderr.read_to_end(&mut err).unwrap();
    Output {
        status,
        stdout: out,
        stderr: err,
    }
}

fn checked(command: &mut Command) -> Output {
    let out = output(command);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn external(program: &str, connection: &smolmachines::SshConnection) -> Command {
    let mut command = Command::new(program);
    command.arg("-F").arg(&connection.config).args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=30",
    ]);
    // The generated connection must restore its context in an unrelated shell.
    for key in [
        "SMOLVM_DATA_DIR",
        "SMOLVM_CONFIG",
        "SMOLVM_AGENT_ROOTFS",
        "SMOLVM_LIB_DIR",
        "SMOLVM_BOOT_BINARY",
    ] {
        command.env_remove(key);
    }
    command
}

#[test]
#[ignore = "boots an isolated machine; requires a hypervisor, matching guest rootfs, SSH and SFTP"]
fn ssh_and_sftp_cold_start_the_pinned_local_environment() {
    let fixture = Fixture(
        tempfile::Builder::new()
            .prefix(".smol-ssh-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap(),
    );
    fixture.ok(&[
        "machine",
        "create",
        "--local",
        "--name",
        NAME,
        "--image",
        "public.ecr.aws/docker/library/alpine:3.21",
        "--cpus",
        "1",
        "--mem",
        "512",
        "--storage",
        "1",
        "--overlay",
        "1",
        "--net",
    ]);
    let connection: smolmachines::SshConnection = serde_json::from_slice(
        &fixture
            .ok(&["machine", "ssh-config", "--local", "--name", NAME])
            .stdout,
    )
    .unwrap();
    assert!(!fixture
        .db()
        .unwrap()
        .get_vm(NAME)
        .unwrap()
        .unwrap()
        .is_process_alive());

    // A saved local identity must not route its start to a cloud move target.
    fixture
        .db()
        .unwrap()
        .update_vm_durable(NAME, |record| {
            record
                .labels
                .insert("smol.moved-to".into(), "do-not-contact-cloud".into());
            record
                .labels
                .insert("smol.moved-to-id".into(), "mach-do-not-contact".into());
        })
        .unwrap();
    let first = checked(
        external("ssh", &connection)
            .arg(&connection.host)
            .arg("printf cold-start-ok"),
    );
    assert_eq!(first.stdout, b"cold-start-ok");
    let exit = output(
        &mut fixture.command(&["machine", "ssh", "--local", "--name", NAME, "--", "exit 17"]),
    );
    assert_eq!(exit.status.code(), Some(17));

    let bytes: Vec<u8> = (0..256 * 1024).map(|n| (n % 251) as u8).collect();
    let source = fixture.0.path().join("upload");
    let target = fixture.0.path().join("download");
    std::fs::write(&source, &bytes).unwrap();
    let mut batch = tempfile::NamedTempFile::new_in(fixture.0.path()).unwrap();
    writeln!(
        batch,
        "put -pf \"{}\" /root/ssh-roundtrip\nget /root/ssh-roundtrip \"{}\"",
        source.display(),
        target.display()
    )
    .unwrap();
    checked(
        external("sftp", &connection)
            .arg("-b")
            .arg(batch.path())
            .arg(&connection.host),
    );
    assert_eq!(std::fs::read(target).unwrap(), bytes);

    fixture.ok(&["machine", "stop", "--local", "--name", NAME]);
    let second = checked(
        external("ssh", &connection)
            .arg(&connection.host)
            .arg("cat /root/ssh-roundtrip"),
    );
    assert_eq!(second.stdout, bytes);
    fixture.ok(&["_engine", "machine", "delete", "--name", NAME, "--force"]);
    assert!(fixture.db().unwrap().get_vm(NAME).unwrap().is_none());
    let remaining: Vec<serde_json::Value> =
        serde_json::from_slice(&fixture.ok(&["_engine", "machine", "ls", "--json"]).stdout)
            .unwrap();
    assert!(
        remaining.is_empty(),
        "internal setup machines were left behind: {remaining:?}"
    );
}
