"""Verify the native error mapping produces specific codes (parity with Node).

Before the mapping port, every engine error collapsed to SMOLVM_ERROR. Now a
missing machine should surface as a specific code (NOT_FOUND), parsed by
`wrap_native_error` into a typed SmolError.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))

from smol import _native  # type: ignore[attr-defined]  # noqa: E402
from smol.errors import SmolError, wrap_native_error  # noqa: E402

# Codes that prove the variant match ran (i.e. not the generic fallback).
SPECIFIC = {
    "NOT_FOUND", "INVALID_STATE", "HYPERVISOR_UNAVAILABLE", "CONFLICT",
    "STORAGE_ERROR", "MOUNT_ERROR", "CONFIG_ERROR", "COMMAND_FAILED",
    "KVM_UNAVAILABLE",
}

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
    # Connecting to a machine that doesn't exist -> engine VmNotFound.
    raised: SmolError | None = None
    try:
        _native.Machine.connect("does-not-exist-zzz-999")
    except Exception as e:  # noqa: BLE001
        raised = wrap_native_error(e)

    check("error_raised", raised is not None)
    if raised is not None:
        print(f"  code={raised.code!r} msg={str(raised)!r}")
        check("specific_code_not_generic_fallback", raised.code in SPECIFIC,
              f"got {raised.code!r} (expected a specific mapped code, not SMOLVM_ERROR)")
        check("missing_machine_is_not_found", raised.code == "NOT_FOUND",
              f"got {raised.code!r}")

    print(f"\n{passed} passed, {failed} failed")
    print("RESULT=PASS" if failed == 0 else "RESULT=FAIL")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
