"""Local e2e for LIVE streaming exec — proves output arrives incrementally, not
buffered. Needs the native build + boot env (SMOLVM_BOOT_BINARY, SMOLVM_LIB_DIR).
"""

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import Machine, MachineConfig, ResourceSpec  # noqa: E402

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
    with Machine.create(MachineConfig(resources=ResourceSpec(cpus=1, memory_mb=512))) as m:
        events = []
        for ev in m.exec_stream(["sh", "-c", "echo AAA; sleep 1; echo BBB"]):
            events.append((time.monotonic(), ev))

        stdout = "".join(e.get("data", "") for _, e in events if e.get("kind") == "stdout")
        check("stdout contains AAA", "AAA" in stdout, repr(stdout))
        check("stdout contains BBB", "BBB" in stdout, repr(stdout))

        exits = [e for _, e in events if e.get("kind") == "exit"]
        check("single exit event, code 0", len(exits) == 1 and exits[0].get("exit_code") == 0, str(exits))

        # Incrementality: the AAA and BBB stdout events must be separated in time
        # (~1s sleep between them). If output were buffered, both would arrive at
        # the end with ~no gap.
        t_a = next((t for t, e in events if e.get("kind") == "stdout" and "AAA" in e.get("data", "")), None)
        t_b = next((t for t, e in events if e.get("kind") == "stdout" and "BBB" in e.get("data", "")), None)
        gap = (t_b - t_a) if (t_a is not None and t_b is not None) else -1.0
        check("output streamed incrementally (>=0.5s gap)", gap >= 0.5, f"gap={gap:.3f}s")

    print(f"\n{passed} passed, {failed} failed")
    print("RESULT=PASS" if failed == 0 else "RESULT=FAIL")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
