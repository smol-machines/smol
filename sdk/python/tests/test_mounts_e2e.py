"""Local e2e for host-directory mounts (parity with the Node SDK).

Proves the native constructor now wires `mounts` into MachineSpec: a host file
becomes visible inside the guest, and a read-only mount rejects writes.

Needs the native build + boot env (SMOLVM_BOOT_BINARY, SMOLVM_LIB_DIR).
"""

import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import Machine, MachineConfig, MountSpec, ResourceSpec  # noqa: E402

passed = 0
failed = 0


def check(name: str, cond: bool, detail: str = "") -> None:
    global passed, failed
    if cond:
        passed += 1
        print(f"PASS {name}")
    else:
        failed += 1
        print(f"FAIL {name} {detail}")


def main() -> int:
    host_dir = tempfile.mkdtemp(prefix="smolmount-")
    Path(host_dir, "hello.txt").write_text("MOUNT_OK")

    with Machine.create(
        MachineConfig(
            mounts=[MountSpec(source=host_dir, target="/mnt/host", readonly=True)],
            resources=ResourceSpec(cpus=1, memory_mb=512),
        )
    ) as m:
        r = m.exec(["cat", "/mnt/host/hello.txt"])
        check("ro_mount_readable", r.exit_code == 0 and r.stdout.strip() == "MOUNT_OK",
              f"exit={r.exit_code} out={r.stdout!r} err={r.stderr!r}")

        w = m.exec(["sh", "-c", "echo nope > /mnt/host/new.txt"])
        check("ro_mount_blocks_write", w.exit_code != 0, f"exit={w.exit_code} (expected non-zero)")
        check("ro_mount_no_leak", not Path(host_dir, "new.txt").exists(),
              "write leaked to host despite read-only mount")

    # Second machine: a writable mount round-trips back to the host.
    host_dir2 = tempfile.mkdtemp(prefix="smolmount-rw-")
    with Machine.create(
        MachineConfig(
            mounts=[MountSpec(source=host_dir2, target="/mnt/rw", readonly=False)],
            resources=ResourceSpec(cpus=1, memory_mb=512),
        )
    ) as m:
        w = m.exec(["sh", "-c", "echo FROM_GUEST > /mnt/rw/out.txt"])
        check("rw_mount_writable", w.exit_code == 0, f"exit={w.exit_code} err={w.stderr!r}")
        check("rw_mount_visible_on_host",
              Path(host_dir2, "out.txt").exists()
              and Path(host_dir2, "out.txt").read_text().strip() == "FROM_GUEST",
              "guest write not visible on host")

    # Staged mode deliberately hides guest writes until explicit sync, then
    # synchronizes the final state again during graceful stop.
    host_dir3 = tempfile.mkdtemp(prefix="smolmount-staged-")
    Path(host_dir3, "state.txt").write_text("initial\n")
    m = Machine.create(
        MachineConfig(
            name=f"staged-python-{os.getpid()}",
            persistent=True,
            mounts=[MountSpec(source=host_dir3, target="/mnt/staged", staged=True)],
            resources=ResourceSpec(cpus=1, memory_mb=512),
        )
    )
    try:
        first = m.exec(["sh", "-c", "echo explicit > /mnt/staged/state.txt"])
        check("staged_write_succeeds", first.exit_code == 0, first.stderr)
        check("staged_write_isolated", Path(host_dir3, "state.txt").read_text() == "initial\n")
        m.sync()
        check("staged_explicit_sync", Path(host_dir3, "state.txt").read_text() == "explicit\n")
        second = m.exec(["sh", "-c", "echo stopped > /mnt/staged/state.txt"])
        check("staged_second_write", second.exit_code == 0, second.stderr)
        m.stop()
        check("staged_stop_sync", Path(host_dir3, "state.txt").read_text() == "stopped\n")
    finally:
        m.delete()

    print(f"\n{passed} passed, {failed} failed")
    print("RESULT=PASS" if failed == 0 else "RESULT=FAIL")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
