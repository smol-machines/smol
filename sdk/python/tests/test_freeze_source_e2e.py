"""Real local test: a branch can leave its source frozen as a reusable base."""

import os
import time

import pytest

from smol import Machine, MachineConfig, ResourceSpec, SmolError


def test_a_frozen_source_keeps_branching_from_the_same_state() -> None:
    suffix = f"{os.getpid()}-{time.time_ns()}"
    source = Machine.create(
        MachineConfig(
            name=f"py-freeze-source-{suffix}",
            image="alpine:3.20",
            resources=ResourceSpec(cpus=1, memory_mb=512, network=True),
            persistent=True,
            branchable=True,
        )
    )
    children: list[Machine] = []
    try:
        source.write_file("/dev/shm/ram-marker", "FROZEN-STATE")
        children.append(source.branch(f"py-freeze-a-{suffix}", freeze_source=True))

        # A frozen source is a branch base; it does not run commands.
        with pytest.raises(SmolError):
            source.exec(["true"])

        children.append(source.branch(f"py-freeze-b-{suffix}", freeze_source=True))
        children.extend(
            source.branch_batch(
                names=[f"py-freeze-c-{suffix}", f"py-freeze-d-{suffix}"],
                freeze_source=True,
            )
        )
        for child in children:
            assert child.exec(["cat", "/dev/shm/ram-marker"]).stdout.strip() == "FROZEN-STATE"
    finally:
        for child in reversed(children):
            try:
                child.delete()
            except Exception:  # noqa: BLE001 - keep cleaning up
                pass
        source.delete()
