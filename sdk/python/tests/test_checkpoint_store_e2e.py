"""Real periodic-checkpoint lifecycle through the public Python SDK."""

import shutil
import tempfile
import time
from pathlib import Path

from smol import Machine, MachineConfig, ResourceSpec


def main() -> None:
    suffix = f"{time.time_ns()}"
    root = Path(tempfile.mkdtemp(prefix="smol-python-checkpoint-"))
    store = root / "store"
    first = root / "first.smolcheckpoint"
    second = root / "second.smolcheckpoint"
    portable = root / "second-portable.smolcheckpoint"
    source = None
    restored = None
    try:
        source = Machine.create(
            MachineConfig(
                name=f"python-checkpoint-source-{suffix}",
                persistent=True,
                branchable=True,
                resources=ResourceSpec(cpus=2, memory_mb=1024),
            )
        )
        source.write_file("/dev/shm/ram-marker", b"RAM-1")
        source.write_file("/workspace/disk-marker", b"DISK-1")
        initial = source.checkpoint(str(first), store=str(store))
        assert initial.size_bytes > 0

        source.write_file("/dev/shm/ram-marker", b"RAM-2")
        source.write_file("/workspace/disk-marker", b"DISK-2")
        incremental = source.checkpoint(str(second), store=str(store))
        assert incremental.reused_bytes and incremental.reused_bytes > 0

        shutil.rmtree(first)
        assert Machine.prune_checkpoint_store(str(store)) > 0
        assert Machine.export_checkpoint(str(second), str(portable)) > 0
        assert portable.is_file()

        restored = Machine.restore_checkpoint(
            str(second), f"python-checkpoint-restored-{suffix}"
        )
        assert restored.read_file("/dev/shm/ram-marker") == b"RAM-2"
        assert restored.read_file("/workspace/disk-marker") == b"DISK-2"
        assert source.exec(["echo", "SOURCE-ALIVE"]).stdout.strip() == "SOURCE-ALIVE"
        print("checkpoint-store-e2e: passed")
    finally:
        if restored is not None:
            restored.delete()
        if source is not None:
            source.delete()
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
