"""Tests for the verifiers runtime adapter (`smol.verifiers`).

The pure policy->egress mapping runs everywhere; the Runtime-contract checks run
only when `verifiers` is installed (otherwise the stub-raises path is verified),
so this passes on a stock runner without verifiers.
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

from smol.verifiers import SmolRuntime, _HAS_VERIFIERS, _resources  # noqa: E402

passed = 0
failed = 0


def check(name: str, cond: bool, detail: str = "") -> None:
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok {name}")
    else:
        failed += 1
        print(f"  FAIL {name}: {detail}")


def _stub_raises_import_error() -> bool:
    try:
        SmolRuntime()
    except ImportError:
        return True
    except Exception:  # noqa: BLE001
        return False
    return False


def main() -> int:
    # --- pure config -> smol ResourceSpec mapping (no verifiers needed) ---
    rs = _resources(cpu=2, memory_gb=4, disk_gb=10, network_access=True)
    check("cpu/memory/disk mapped (GB->MB, cores)", rs.cpus == 2 and rs.memory_mb == 4096 and rs.storage_gb == 10, f"{rs.cpus}/{rs.memory_mb}/{rs.storage_gb}")
    check("network_access=True -> network on", rs.network is True and rs.allow_hosts is None)
    check("network_access=False -> network off", _resources(1, 2, 5, False).network is False)
    scoped = _resources(1, 2, 5, False, allow_hosts=["api.anthropic.com"])
    check(
        "allow_hosts scopes egress and forces network on",
        scoped.network is True and scoped.allow_hosts == ["api.anthropic.com"],
        f"{scoped.network}/{scoped.allow_hosts}",
    )

    if _HAS_VERIFIERS:
        from verifiers.v1.runtimes.base import Runtime  # noqa: E402

        from smol.verifiers import SmolConfig  # noqa: E402

        check("SmolRuntime is a verifiers Runtime", issubclass(SmolRuntime, Runtime))
        for m in ("start", "run", "read", "write", "teardown", "cleanup", "expose"):
            check(f"SmolRuntime implements {m}", callable(getattr(SmolRuntime, m, None)))
        # Construct it for real — exercises SmolConfig + the SmolRuntimeInfo
        # (SmolConfig, BaseRuntimeInfo) multiple inheritance + model_dump.
        rt = SmolRuntime(SmolConfig(image="python:3.12-slim", golden="mach-abc"))
        check("constructs and carries config", rt.config.image == "python:3.12-slim" and rt.config.golden == "mach-abc")
        check("info reflects the config", rt.info.image == "python:3.12-slim")
        check("is a remote runtime", rt.is_local is False)
    else:
        print("  (verifiers not installed — Runtime-subclass checks skipped)")
        check(
            "SmolRuntime stub raises a helpful ImportError without verifiers",
            _stub_raises_import_error(),
        )

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
