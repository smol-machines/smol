"""End-to-end checks for embedding the SDK in a long-running host daemon:
open without booting, delete by name, exec stdin, sizes and ports in list(),
stops fenced against running work, live port publishing, pack and
create-from-pack, and disks under 20 GiB.

Needs a bundled engine (SMOLVM_BOOT_BINARY, SMOLVM_LIB_DIR,
SMOLVM_AGENT_ROOTFS); skipped otherwise. Run: ``python tests/test_host_daemon_e2e.py``.
"""

from __future__ import annotations

import os
import socket
import sys
import tempfile
import threading
import time

from smol import ConnectOptions, ExecOptions, Machine, MachineConfig, PortSpec, ResourceSpec, SmolError

LOCAL = ConnectOptions(target="local")
IMAGE = "alpine:3.20"
results: list[tuple[str, bool, str]] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    results.append((name, ok, detail))
    print(f"{'PASS' if ok else 'FAIL'} {name}{f' ({detail})' if detail else ''}", flush=True)


def code(e: BaseException) -> str:
    return e.code if isinstance(e, SmolError) else repr(e)


def connect_to(port: int) -> str:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=4) as s:
            return s.recv(64).decode().strip()
    except OSError:
        return "refused"


def main() -> int:
    for n in ("hdp-a", "hdp-b", "hdp-small", "hdp-from-pack"):
        Machine.delete_by_name(n, LOCAL)

    a = Machine.create(MachineConfig(name="hdp-a", image=IMAGE, network=True, persistent=True), LOCAL)
    a.stop()
    check("open() a stopped machine reports stopped", Machine.open("hdp-a", LOCAL).state() == "stopped")
    try:
        Machine.open("hdp-no-such", LOCAL)
        check("open() of a missing machine raises NOT_FOUND", False)
    except SmolError as e:
        check("open() of a missing machine raises NOT_FOUND", e.code == "NOT_FOUND", e.code)

    m = Machine.connect("hdp-a", ConnectOptions(target="local", wait_for_ports=False))
    out = m.exec(["sh", "-s"], ExecOptions(stdin="echo from-script\n"))
    check("exec stdin runs a piped script", out.stdout.strip() == "from-script", repr(out.stdout))

    outcome: dict[str, str] = {}

    def long_exec() -> None:
        try:
            m.exec(["sh", "-c", "sleep 30"])
            outcome["long"] = "finished"
        except SmolError as e:
            outcome["long"] = e.code

    t = threading.Thread(target=long_exec)
    t.start()
    time.sleep(0.5)
    m.stop()
    t.join(30)
    check("stop() during exec raises INTERRUPTED", outcome.get("long") == "INTERRUPTED", outcome.get("long", ""))

    m = Machine.connect("hdp-a", ConnectOptions(target="local", wait_for_ports=False))

    def short_exec() -> None:
        outcome["short"] = m.exec(["sh", "-c", "sleep 2; echo done"]).stdout.strip()

    t = threading.Thread(target=short_exec)
    t.start()
    time.sleep(0.3)
    m.stop(drain_secs=10)
    t.join(30)
    check("stop(drain_secs) lets a running exec finish", outcome.get("short") == "done", outcome.get("short", ""))

    Machine.delete_by_name("hdp-a", LOCAL)
    check("delete_by_name removes a stopped machine", not any(x.name == "hdp-a" for x in Machine.list(LOCAL)))
    Machine.delete_by_name("hdp-never-existed", LOCAL)
    check("delete_by_name of a missing machine succeeds", True)

    b = Machine.create(
        MachineConfig(
            name="hdp-b",
            image=IMAGE,
            persistent=True,
            ports=[PortSpec(host=18531, guest=8080)],
            resources=ResourceSpec(cpus=2, memory_mb=1024, network=True),
            wait_for_ports=False,
        ),
        LOCAL,
    )
    row = next(x for x in Machine.list(LOCAL) if x.name == "hdp-b")
    check(
        "list() reports sizes and ports",
        row.resources is not None
        and row.resources.cpus == 2
        and row.resources.memory_mib == 1024
        and any(p.guest == 8080 for p in (row.ports or [])),
        f"{row.resources} {row.ports}",
    )
    b.exec(["sh", "-c", "(while true; do echo hello-3000 | nc -l -p 3000; done) >/dev/null 2>&1 &"])
    published = b.publish(3000)
    reply = connect_to(published.host or 0)
    check("publish() on a running machine reaches the guest", reply == "hello-3000", f"{published.host} -> {reply}")
    b.unpublish(guest=3000)
    check("unpublish() stops accepting", connect_to(published.host or 0) == "refused")

    b.exec(["sh", "-c", "echo packed-state > /root/marker"])
    b.stop()
    with tempfile.TemporaryDirectory() as d:
        artifact = b.pack(os.path.join(d, "rig"))
        check("pack() writes a .smolmachine", artifact.endswith(".smolmachine"), artifact)
        restored = Machine.create(
            MachineConfig(name="hdp-from-pack", image=artifact, wait_for_ports=False), LOCAL
        )
        marker = restored.exec(["cat", "/root/marker"]).stdout.strip()
        check("a machine created from the pack has its state", marker == "packed-state", repr(marker))
        restored.delete()

        # Options on a pack are applied the way they are on an image, and a
        # branchable pack machine can be branched.
        configured = Machine.create(
            MachineConfig(
                name="hdp-pack-options",
                image=artifact,
                env={"PACK_OPTION": "set"},
                workdir="/tmp",
                labels={"suite": "host-daemon"},
                resources=ResourceSpec(cpus=1, memory_mb=768),
                branchable=True,
                wait_for_ports=False,
            ),
            LOCAL,
        )
        seen = configured.exec(["sh", "-c", 'echo "$PACK_OPTION $(pwd)"']).stdout.strip()
        check("env and workdir reach a pack machine", seen == "set /tmp", repr(seen))
        row = next(x for x in Machine.list(LOCAL) if x.name == "hdp-pack-options")
        check(
            "labels and resources reach a pack machine",
            (row.labels or {}).get("suite") == "host-daemon"
            and row.resources is not None
            and row.resources.cpus == 1
            and row.resources.memory_mib == 768,
            f"{row.labels} {row.resources}",
        )
        branch = configured.fork("hdp-pack-branch")
        branch_marker = branch.exec(["cat", "/root/marker"]).stdout.strip()
        check("a branchable pack machine branches", branch_marker == "packed-state", repr(branch_marker))
        branch.delete()
        configured.delete()
    Machine.delete_by_name("hdp-b", LOCAL)

    small = Machine.create(
        MachineConfig(
            name="hdp-small",
            image=IMAGE,
            resources=ResourceSpec(storage_gb=8, network=True),
            wait_for_ports=False,
        ),
        LOCAL,
    )
    kib = int(small.exec(["sh", "-c", "df -k /storage | tail -1 | awk '{print $2}'"]).stdout.strip() or 0)
    check("an 8 GiB disk boots at its size", 7 * 2**20 < kib <= 8 * 2**20, f"{kib} KiB")
    small.delete()

    failed = sum(1 for _, ok, _ in results if not ok)
    print(f"RESULT pass={len(results) - failed} fail={failed}")
    return 1 if failed else 0


def test_host_daemon_e2e() -> None:
    import pytest

    if not os.environ.get("SMOLVM_BOOT_BINARY"):
        pytest.skip("needs a bundled engine (SMOLVM_BOOT_BINARY)")
    assert main() == 0


if __name__ == "__main__":
    sys.exit(main())
