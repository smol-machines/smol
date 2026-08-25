"""Local embedded e2e — boots a real microVM via the native extension.

Mirrors the Node ``test/e2e.ts``. Requires the native ext built
(``maturin develop``) and the engine env (``SMOLVM_BOOT_BINARY`` pointing at a
signed smol/smolvm helper, ``SMOLVM_LIB_DIR`` at the libkrun dir). Skips cleanly
if the native ext isn't available.
"""

import asyncio
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import (  # noqa: E402
    AsyncMachine,
    ExecutionError,
    Machine,
    MachineConfig,
    ResourceSpec,
)


async def concurrent_exec_probe():
    golden = await AsyncMachine.create(
        MachineConfig(
            checkpoint=True,
            resources=ResourceSpec(cpus=2, memory_mb=1024),
        )
    )
    clones = []
    try:
        clones = await golden.fork_batch(
            names=[f"{golden.name}-concurrency-a", f"{golden.name}-concurrency-b"]
        )
        started = time.perf_counter()
        results = await asyncio.gather(
            *(clone.exec(["sleep", "2"]) for clone in clones)
        )
        return time.perf_counter() - started, results
    finally:
        if clones:
            await asyncio.gather(
                *(clone.delete() for clone in clones), return_exceptions=True
            )
        await golden.delete()


def main() -> int:
    passed = failed = 0

    def check(name: str, cond: bool, detail: str = ""):
        nonlocal passed, failed
        if cond:
            passed += 1
            print(f"  ok {name}")
        else:
            failed += 1
            print(f"  FAIL {name} {detail}")

    print("smol Python SDK local e2e\n")
    m = Machine.create(
        MachineConfig(resources=ResourceSpec(cpus=2, memory_mb=1024, network=True))
    )
    try:
        check("machine has a name", bool(m.name), m.name)

        echo = m.exec(["echo", "hello-from-vm"])
        check("exec exit 0", echo.exit_code == 0, echo.stderr)
        check("exec stdout", echo.stdout.strip() == "hello-from-vm", repr(echo.stdout))
        check("exec success flag", echo.success is True)

        py = m.run("python:3.12-alpine", ["python", "-c", "print(2 ** 10)"])
        check("run exit 0", py.exit_code == 0, py.stderr)
        check("run stdout = 1024", py.stdout.strip() == "1024", repr(py.stdout))

        threw = False
        try:
            m.exec(["sh", "-c", "exit 7"]).assert_success(["false"])
        except ExecutionError as e:
            threw = e.exit_code == 7
        check("assert_success raises ExecutionError(7)", threw)

        payload = b"roundtrip-payload"
        m.write_file("/tmp/smol-e2e.txt", payload)
        back = m.read_file("/tmp/smol-e2e.txt")
        check("file round-trip", back == payload, repr(back))

        imgs = m.list_images()
        check("list_images includes python", any("python" in i.reference for i in imgs))
    finally:
        m.delete()

    elapsed, results = asyncio.run(concurrent_exec_probe())
    check("concurrent execs exit 0", all(result.exit_code == 0 for result in results))
    check(
        "concurrent execs overlap off-GIL",
        elapsed < 3.5,
        f"two sleep-2 execs took {elapsed:.3f}s",
    )

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
