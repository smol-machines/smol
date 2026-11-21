//! Resize through the real CLI, using an isolated state root and no VMs by default.
#![cfg(unix)]

use smolvm::{config::RecordState, db::SmolvmDb};
use std::{
    path::PathBuf,
    process::{Command, Output},
};

const NAME: &str = "resize-before-boot";
const GIB: u64 = 1 << 30;

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            // macOS's per-user temporary directory can exceed the Unix-socket
            // path budget once the machine's cache directory is appended.
            root: tempfile::Builder::new()
                .prefix("sr-")
                .tempdir_in("/tmp")
                .unwrap(),
        };
        fixture.ok(&[
            "machine",
            "create",
            "--name",
            NAME,
            "--cpus",
            "1",
            "--mem",
            "512",
            "--storage",
            "1",
            "--overlay",
            "1",
        ]);
        fixture
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut local_args = args.to_vec();
        let insert = local_args
            .iter()
            .position(|arg| *arg == "--")
            .unwrap_or(local_args.len());
        local_args.insert(insert, "--local");
        Command::new(env!("CARGO_BIN_EXE_smol"))
            .args(local_args)
            .env("SMOLVM_DATA_DIR", self.root.path())
            .env("SMOLVM_CONFIG", self.root.path().join("settings.toml"))
            .env(
                "SMOLVM_AGENT_ROOTFS",
                std::env::var_os("SMOLVM_AGENT_ROOTFS")
                    .unwrap_or_else(|| self.root.path().join("no-rootfs-needed").into_os_string()),
            )
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn db(&self) -> SmolvmDb {
        let base = if cfg!(target_os = "macos") {
            "Library/Application Support"
        } else {
            ".local/share"
        };
        SmolvmDb::open_at(&self.root.path().join(base).join("smolvm/server/smolvm.db")).unwrap()
    }
    fn disks(&self) -> PathBuf {
        let base = if cfg!(target_os = "macos") {
            "Library/Caches"
        } else {
            ".cache"
        };
        self.root
            .path()
            .join(base)
            .join("smolvm/vms")
            .join(smolvm::agent::vm_dir_hash(NAME))
    }
    /// Whether neither disk image exists yet. A created machine already owns
    /// its directory (it holds the name binding), but no disk is allocated
    /// until the machine first boots.
    fn no_disks(&self) -> bool {
        [
            smolvm::storage::STORAGE_DISK_FILENAME,
            smolvm::storage::OVERLAY_DISK_FILENAME,
        ]
        .iter()
        .all(|disk| !self.disks().join(disk).exists())
    }
    fn sizes(&self) -> (Option<u64>, Option<u64>) {
        let record = self.db().get_vm(NAME).unwrap().unwrap();
        (record.storage_gb, record.overlay_gb)
    }
    fn update(&self, storage: &str, overlay: &str) -> Output {
        self.run(&[
            "machine",
            "update",
            "--name",
            NAME,
            "--storage",
            storage,
            "--overlay",
            overlay,
            "--mem",
            "768",
        ])
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["machine", "rm", "--name", NAME, "--yes"]);
    }
}

#[test]
fn public_update_plans_disk_sizes_before_first_boot() {
    let f = Fixture::new();
    assert!(f.no_disks());
    let output = f.update("2", "3");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(f.sizes(), (Some(2), Some(3)));
    assert_eq!(f.db().get_vm(NAME).unwrap().unwrap().mem, 768);
    assert!(
        f.no_disks(),
        "editing a fresh machine must not allocate disks"
    );
    f.ok(&[
        "machine",
        "update",
        "--name",
        NAME,
        "--storage",
        "3",
        "--overlay",
        "4",
    ]);
    f.ok(&[
        "machine",
        "update",
        "--name",
        NAME,
        "--storage",
        "3",
        "--overlay",
        "4",
    ]);
    assert_eq!(f.sizes(), (Some(3), Some(4)));
    let shrink = f.update("2", "5");
    assert!(!shrink.status.success());
    assert_eq!(
        f.sizes(),
        (Some(3), Some(4)),
        "invalid combined resize must not partially save"
    );
    assert!(f.no_disks());
}

#[test]
fn a_missing_stopped_disk_fails_before_any_disk_grows_or_settings_change() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.disks()).unwrap();
    let storage = f.disks().join(smolvm::storage::STORAGE_DISK_FILENAME);
    let file = std::fs::File::create(&storage).unwrap();
    file.set_len(GIB).unwrap();
    f.db()
        .update_vm(NAME, |r| r.state = RecordState::Stopped)
        .unwrap();
    let output = f.update("2", "2");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("inspect resize disk"));
    assert_eq!(std::fs::metadata(storage).unwrap().len(), GIB);
    assert_eq!(f.sizes(), (Some(1), Some(1)));
    assert_eq!(f.db().get_vm(NAME).unwrap().unwrap().mem, 512);
}

#[test]
fn clone_dependencies_and_prepared_image_storage_remain_protected() {
    let f = Fixture::new();
    let mut clone = f.db().get_vm(NAME).unwrap().unwrap();
    clone.name = "dependent-clone".into();
    clone.golden = Some(NAME.into());
    f.db().insert_vm(&clone.name, &clone).unwrap();
    let output = f.update("2", "2");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("fork base"));
    assert_eq!(f.sizes(), (Some(1), Some(1)));
    f.db().remove_vm(&clone.name).unwrap();
    f.db()
        .update_vm(NAME, |r| r.packed_image_on_storage = true)
        .unwrap();
    assert!(!f.update("2", "2").status.success());
    assert_eq!(f.sizes(), (Some(1), Some(1)));
}

/// Run with installed libkrun/rootfs and a signed test CLI on macOS.
#[test]
#[ignore = "boots real machines; requires a working hypervisor and agent rootfs"]
fn first_boot_uses_planned_sizes_and_later_growth_preserves_data() {
    let f = Fixture::new();
    let output = f.update("2", "3");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(f.no_disks());
    f.ok(&["machine", "start", "--name", NAME]);
    assert_eq!(
        std::fs::metadata(f.disks().join(smolvm::storage::STORAGE_DISK_FILENAME))
            .unwrap()
            .len(),
        2 * GIB
    );
    assert_eq!(
        std::fs::metadata(f.disks().join(smolvm::storage::OVERLAY_DISK_FILENAME))
            .unwrap()
            .len(),
        3 * GIB
    );
    f.ok(&[
        "machine",
        "exec",
        "--name",
        NAME,
        "--",
        "sh",
        "-ec",
        "printf storage > /workspace/resize-marker; printf overlay > /root/resize-marker",
    ]);
    f.ok(&["machine", "stop", "--name", NAME]);
    let output = f.update("3", "4");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    f.ok(&["machine", "start", "--name", NAME]);
    f.ok(&["machine", "exec", "--name", NAME, "--", "sh", "-ec", "test \"$(cat /workspace/resize-marker)\" = storage; test \"$(cat /root/resize-marker)\" = overlay"]);
    f.ok(&["machine", "stop", "--name", NAME]);
}
