"""Real local image-workload and fork smoke test."""

from __future__ import annotations

import os
import socket
import time

from smol import Machine, MachineConfig, PortSpec, ResourceSpec


def available_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def test_forked_image_workload_ports_and_overlays() -> None:
    suffix = f"{os.getpid()}-{time.time_ns()}"
    golden = Machine.create(
        MachineConfig(
            name=f"py-golden-{suffix}",
            image="nginx:1.27-alpine",
            ports=[PortSpec(host=available_port(), guest=80)],
            resources=ResourceSpec(cpus=2, memory_mb=1024, network=True),
            persistent=True,
            forkable=True,
        )
    )
    clones: list[Machine] = []
    try:
        assert b"Welcome to nginx" in golden.request(80)
        first = golden.fork(f"py-clone-one-{suffix}")
        clones.append(first)
        second = golden.fork(f"py-clone-two-{suffix}")
        clones.append(second)

        assert first.endpoint(80).http_url != second.endpoint(80).http_url
        first.write_file("/usr/share/nginx/html/index.html", b"clone-one")
        assert first.read_file("/usr/share/nginx/html/index.html") == b"clone-one"
        assert first.request(80).strip() == b"clone-one"
        assert b"Welcome to nginx" in second.request(80)
        second.stop()
        second.start()
        assert b"Welcome to nginx" in second.request(80)
    finally:
        for clone in clones:
            clone.delete()
        golden.delete()
