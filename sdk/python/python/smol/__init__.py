"""smol — embed isolated microVM sandboxes directly in your Python code.

Same API local (embedded engine, no server) or cloud (smolfleet) — the backend
is chosen via :class:`ConnectOptions` / ``SMOL_CLOUD_TOKEN``. Mirrors the Node SDK.

>>> from smol import Machine, MachineConfig
>>> with Machine.create(MachineConfig(image="python:3.12")) as m:    # doctest: +SKIP
...     print(m.run("python:3.12", ["python", "-c", "print(2 ** 10)"]).stdout)
"""

from __future__ import annotations

import os as _os
from pathlib import Path as _Path


def _wire_bundled_native() -> None:
    """Point the embedded engine at the boot helper + libs bundled in this wheel.

    Mirrors the Node SDK's ``assets.js``. On macOS the hypervisor entitlement
    lives on ``smol-vmm`` (spawned as a subprocess), so the engine must launch the
    bundled, signed helper rather than call the hypervisor from the unentitled
    python process. No-op (cloud still works) if the helper isn't present.
    """
    pkg = _Path(__file__).resolve().parent
    helper = pkg / ("smol-vmm.exe" if _os.name == "nt" else "smol-vmm")
    if helper.exists():
        _os.environ.setdefault("SMOLVM_BOOT_BINARY", str(helper))
    # libkrun/libkrunfw are bundled flat next to _native in the package dir.
    _os.environ.setdefault("SMOLVM_LIB_DIR", str(pkg))
    # Bundled guest rootfs tarball — the engine extracts it on first use, so the
    # wheel is fully self-contained (no separate engine install needed). A wheel
    # can't ship a rootfs dir tree (symlinks/modes), so we ship a tarball.
    rootfs_tar = pkg / "agent-rootfs.tar"
    if rootfs_tar.exists() and "SMOLVM_AGENT_ROOTFS" not in _os.environ:
        _os.environ.setdefault("SMOLVM_AGENT_ROOTFS_TAR", str(rootfs_tar))


_wire_bundled_native()

from .errors import (
    ExecutionError,
    InvalidConfigError,
    NotSupportedError,
    SmolError,
    wrap_native_error,
)
from .machine import Machine
from .types import (
    ConnectOptions,
    ExecOptions,
    ExecResult,
    ImageInfo,
    MachineConfig,
    MountSpec,
    PortSpec,
    ResourceSpec,
)

__version__ = "1.3.2"

__all__ = [
    "Machine",
    "MachineConfig",
    "ResourceSpec",
    "MountSpec",
    "PortSpec",
    "ExecOptions",
    "ExecResult",
    "ImageInfo",
    "ConnectOptions",
    "SmolError",
    "NotSupportedError",
    "InvalidConfigError",
    "ExecutionError",
    "wrap_native_error",
]
