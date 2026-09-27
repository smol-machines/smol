"""Per-command user, killing commands, and concurrency on real local machines.

Needs the native build + boot env (SMOLVM_BOOT_BINARY, SMOLVM_LIB_DIR) and
KVM; exits 0 with a skip message without them. Liveness is checked with a
heartbeat file rather than ``ps``: on an image machine each command is its own
container, so one exec cannot see another's processes.
"""

import asyncio
import os
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import AsyncMachine, ExecOptions, Machine, MachineConfig, SmolError, local_availability  # noqa: E402

failures = 0


def check(label: str, ok: bool, detail: str = "") -> None:
    global failures
    if not ok:
        failures += 1
    print(f"  {'PASS' if ok else 'FAIL'}  {label}  ({detail})")


def heartbeat(name: str) -> list[str]:
    return ["sh", "-c", f"mkdir -p /workspace && i=0; while true; do i=$((i+1)); echo $i > /workspace/{name}; sleep 0.2; done"]


def beating(m: Machine, name: str) -> bool:
    def read() -> str:
        return m.exec(["cat", f"/workspace/{name}"]).stdout.strip()

    first = read()
    time.sleep(0.9)
    return first != "" and read() != first


def suite(label: str, m: Machine) -> None:
    print(f"\n== {label}")
    long = m.exec_stream(["sh", "-c", "sleep 5; echo done-long"])
    time.sleep(0.5)
    started = time.monotonic()
    during = m.exec(["echo", "hi"])
    took = time.monotonic() - started
    check("exec stays fast while a stream runs", during.stdout.strip() == "hi" and took < 2, f"{took:.3f}s")
    out = "".join(e.get("data", "") for e in long if e["kind"] == "stdout")
    check("the long stream still completes", out.strip() == "done-long", out.strip())

    stream = m.exec_stream(heartbeat("hb-kill"))
    time.sleep(1)
    check("streamed command runs before the kill", beating(m, "hb-kill"))
    threading.Timer(0.2, stream.kill).start()
    rest = list(stream)
    check("a killed stream ends", rest == [], str(rest))
    time.sleep(0.3)
    check("the killed command is gone", not beating(m, "hb-kill"))

    for event in m.exec_stream(["sh", "-c", "mkdir -p /workspace && i=0; while true; do echo tick; i=$((i+1)); echo $i > /workspace/hb-break; sleep 0.2; done"]):
        if event["kind"] == "stdout":
            break
    time.sleep(0.3)
    check("breaking out of the loop kills the command", not beating(m, "hb-break"))

    started = time.monotonic()
    timed = m.exec(["sleep", "10"], ExecOptions(timeout=1.5))
    took = time.monotonic() - started
    check("a 1.5 s timeout ends the command", took < 6, f"{took:.2f}s exit {timed.exit_code}")

    async def cancel_exec() -> bool:
        am = AsyncMachine(m)
        task = asyncio.create_task(am.exec(heartbeat("hb-async")))
        await asyncio.sleep(1.5)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            return True
        return False

    check("cancelling an async exec raises CancelledError", asyncio.run(cancel_exec()))
    time.sleep(0.3)
    check("the cancelled exec's command is gone", not beating(m, "hb-async"))


def main() -> int:
    if not os.environ.get("SMOLVM_BOOT_BINARY"):
        print("SKIP: SMOLVM_BOOT_BINARY not set (needs a native build and KVM)")
        return 0
    available, code, reason = local_availability()
    check("this host reports it can run local machines", available, f"{code}: {reason}")
    pid = os.getpid()

    bare = Machine.create(MachineConfig(name=f"py-bare-{pid}"))
    try:
        bare.start()
        suite("bare VM", bare)
        try:
            bare.exec(["id", "-u"], ExecOptions(user="nobody"))
            check("a bare VM refuses a per-command user", False, "ran")
        except SmolError as e:
            check("a bare VM refuses a per-command user", "image machine" in str(e), str(e)[:100])
    finally:
        bare.delete()

    image = Machine.create(MachineConfig(name=f"py-img-{pid}", image="alpine", network=True))
    try:
        image.start()
        as_user = image.exec(["id", "-u"], ExecOptions(user="nobody"))
        check("exec runs as the requested user", as_user.stdout.strip() == "65534", as_user.stdout.strip())
        as_root = image.exec(["id", "-u"])
        check("exec without a user still runs as root", as_root.stdout.strip() == "0", as_root.stdout.strip())
        streamed = "".join(e.get("data", "") for e in image.exec_stream(["id", "-u"], ExecOptions(user="nobody")) if e["kind"] == "stdout")
        check("exec_stream runs as the requested user", streamed.strip() == "65534", streamed.strip())
        suite("image machine (alpine)", image)
    finally:
        image.delete()

    print(f"\n{'ALL PASSED' if failures == 0 else f'{failures} FAILED'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
