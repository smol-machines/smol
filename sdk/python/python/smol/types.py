"""Public types for the ``smol`` SDK — backend-agnostic, mirroring ``types.ts``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Literal, Optional

__all__ = [
    "ResourceSpec",
    "MountSpec",
    "PortSpec",
    "MachineConfig",
    "ExecOptions",
    "ExecResult",
    "ImageInfo",
    "ConnectOptions",
    "MachineState",
]

MachineState = str  # "created" | "running" | "stopped"


@dataclass
class ResourceSpec:
    cpus: Optional[int] = None
    """Number of vCPUs."""
    memory_mb: Optional[int] = None
    """Memory in MB."""
    network: Optional[bool] = None
    """Enable outbound network access (TSI). Default: False."""
    storage_gb: Optional[int] = None
    """Storage disk size in GB."""
    overlay_gb: Optional[int] = None
    """Overlay disk size in GB."""
    gpu: Optional[bool] = None
    """Enable GPU acceleration (virtio-gpu/venus). Local target only. Default: False."""
    gpu_vram_mib: Optional[int] = None
    """GPU VRAM in MiB (default: engine default when GPU is enabled). Local target only."""


@dataclass
class MountSpec:
    source: str
    """Absolute path on the host."""
    target: str
    """Absolute path inside the machine."""
    readonly: bool = True


@dataclass
class PortSpec:
    host: int
    guest: int


@dataclass
class MachineConfig:
    """Configuration for creating a machine."""

    name: Optional[str] = None
    """Machine name (auto-generated if omitted)."""
    image: Optional[str] = None
    """Base image. Required for the cloud target; optional for local."""
    mounts: Optional[list[MountSpec]] = None
    ports: Optional[list[PortSpec]] = None
    resources: Optional[ResourceSpec] = None
    persistent: bool = False
    """Keep the machine record after the process exits (local)."""
    auto_stop_seconds: Optional[int] = None
    """Auto-stop after N idle seconds (cloud)."""
    ttl_seconds: Optional[int] = None
    """Delete after N seconds (cloud)."""
    forkable: bool = False
    """Start as a live-RAM fork base (cloud) so the machine can be cloned with
    :meth:`Machine.fork`. The golden and its clones are pinned to one node."""


@dataclass
class ExecOptions:
    env: Optional[dict[str, str]] = None
    workdir: Optional[str] = None
    timeout: Optional[int] = None
    """Timeout in seconds."""


@dataclass
class ExecResult:
    exit_code: int
    stdout: str
    """Captured stdout as text (UTF-8; invalid bytes replaced). For BINARY output,
    read it back with ``read_file()`` instead — this conversion is lossy. Very
    large output (>~20 MB) is rejected; use ``exec_stream`` for that."""
    stderr: str

    @property
    def success(self) -> bool:
        return self.exit_code == 0

    @property
    def output(self) -> str:
        """stdout + stderr concatenated."""
        if self.stderr:
            return self.stdout + ("\n" if self.stdout else "") + self.stderr
        return self.stdout

    def assert_success(self, command: list[str] | str = "") -> "ExecResult":
        """Raise ``ExecutionError`` if the command exited non-zero."""
        if not self.success:
            from .errors import ExecutionError

            raise ExecutionError(command, self.exit_code, self.stdout, self.stderr)
        return self


@dataclass
class ImageInfo:
    reference: str
    digest: str
    size: int
    architecture: str
    os: str


@dataclass
class ConnectOptions:
    """Selects and configures the backend. Local (embedded) is the default."""

    target: Optional[Literal["local", "cloud"]] = None
    base_url: Optional[str] = None
    api_key: Optional[str] = None
