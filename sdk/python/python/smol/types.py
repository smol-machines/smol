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
    "MachineUsageReport",
    "PortableCheckpointInfo",
    "PortEndpoint",
]

# Lifecycle state. Cloud "started" means the VM process launched, not that the
# guest agent or workload is ready; use ready()/wait_until_ready() before work.
MachineState = str  # "created" | "started" | "running" | "stopped"


@dataclass
class ResourceSpec:
    cpus: Optional[int] = None
    """Number of vCPUs."""
    memory_mb: Optional[int] = None
    """Memory in MB."""
    network: Optional[bool] = None
    """Enable outbound network access (TSI). Default: False."""
    allow_cidrs: Optional[list[str]] = None
    """Scope egress to these CIDR ranges. Setting this (or allow_hosts) enables
    networking and restricts it to the listed CIDRs."""
    allow_hosts: Optional[list[str]] = None
    """Scope egress to these hostnames and their subdomains (e.g.
    api.anthropic.com). Setting this (or allow_cidrs) enables networking and
    restricts it to the listed hosts."""
    storage_gb: Optional[int] = None
    """Storage disk size in GB."""
    overlay_gb: Optional[int] = None
    """Overlay disk size in GB."""
    gpu: Optional[bool] = None
    """Enable GPU acceleration (virtio-gpu/venus). Local target only. Default: False."""
    gpu_vram_mib: Optional[int] = None
    """GPU VRAM in MiB (default: engine default when GPU is enabled). Local target only."""
    cuda: Optional[bool] = None
    """Run the guest's unmodified CUDA/PyTorch code on the host's NVIDIA GPU by
    remoting CUDA Driver-API calls to the host over vsock (distinct from ``gpu``,
    which is Vulkan; no CUDA toolkit needed in the image). Local target only."""


@dataclass
class MountSpec:
    source: str
    """Absolute path on the host."""
    target: str
    """Absolute path inside the machine."""
    read_only: bool = False
    """Mount read-only. Default: False (writable), matching the ``smol -v`` CLI."""
    readonly: Optional[bool] = None
    """Deprecated alias for :attr:`read_only`; kept for backwards compatibility."""
    staged: bool = False
    """Use a guest-local working copy and copy changes back on :meth:`Machine.sync`,
    graceful stop, or delete. Local only; incompatible with read-only mode."""

    @property
    def effective_read_only(self) -> bool:
        """Resolve the read-only flag, preferring the deprecated ``readonly``
        alias when explicitly set, else ``read_only``."""
        return self.readonly if self.readonly is not None else self.read_only


@dataclass
class PortSpec:
    """Host/guest port mapping. On cloud, readiness includes the published
    guest port accepting connections."""

    host: int
    guest: int


@dataclass
class MachineConfig:
    """Configuration for creating a machine."""

    name: Optional[str] = None
    """Machine name (auto-generated if omitted)."""
    image: Optional[str] = None
    """Base image. Required for the cloud target; optional for local."""
    command: Optional[list[str]] = None
    """Workload argv overriding the image entrypoint/CMD."""
    mounts: Optional[list[MountSpec]] = None
    ports: Optional[list[PortSpec]] = None
    resources: Optional[ResourceSpec] = None
    network: Optional[bool] = None
    """Give the guest network access. :attr:`ResourceSpec.network` is canonical;
    this is the shape callers reach for first (and the one the Node SDK already
    accepts). Reading only the canonical one meant this was dropped in silence,
    so a config that plainly asked for network produced a machine without it —
    and an image pull then failed with an unreachable-network error that reads
    like a broken VM. ``resources.network`` still wins when both are given, so
    no existing config changes meaning."""
    persistent: bool = False
    """Keep the machine record after the process exits (local)."""
    auto_stop_seconds: Optional[int] = None
    """Auto-stop after N idle seconds (cloud)."""
    ttl_seconds: Optional[int] = None
    """Delete after N seconds (cloud)."""
    ready_timeout_seconds: float = 120.0
    """Maximum time creation waits for the guest and published services to
    become ready. Increase this for large images or heavily prepared sandbox
    workloads; the default preserves the SDK's existing two-minute behavior."""
    branchable: Optional[bool] = None
    """Start as a live-RAM branch source so the machine can produce independent
    copy-on-write children with :meth:`Machine.branch`."""
    forkable: bool = False
    """Deprecated alias for :attr:`branchable`."""
    checkpoint: bool = False
    """Deprecated alias for :attr:`branchable`; checkpoints are durable artifacts."""
    env: Optional[dict[str, str]] = None
    """Environment variables for the image workload launched at create."""
    workdir: Optional[str] = None
    """Working directory for the image workload, set at create. Overrides the
    image's own workdir."""

    def __post_init__(self) -> None:
        # Native/cloud transports retain the compatibility `forkable` field.
        if self.branchable is not None:
            self.forkable = self.branchable
        elif self.checkpoint:
            self.forkable = True
        if self.command is not None and (
            not self.command
            or any(not isinstance(arg, str) or not arg for arg in self.command)
        ):
            raise ValueError("command must be a non-empty list of non-empty strings")
        if self.ready_timeout_seconds <= 0:
            raise ValueError("ready_timeout_seconds must be > 0")


@dataclass
class ExecOptions:
    env: Optional[dict[str, str]] = None
    workdir: Optional[str] = None
    timeout: Optional[int] = None
    """Timeout in seconds."""
    output: Optional[Literal["text", "b64", "both"]] = None
    """Cloud target only: which output encodings the server returns. The default
    carries both the capped UTF-8 text fields and the byte-exact base64 fields;
    ``"text"`` or ``"b64"`` halves the response payload by dropping the other
    family. With ``"text"``, :attr:`ExecResult.stdout_bytes` degrades to the
    lossy text re-encoded; with ``"b64"``, :attr:`ExecResult.stdout` is empty.
    Ignored (both families, as before) on older control planes and locally."""


@dataclass
class ExecResult:
    exit_code: int
    stdout: str
    """Captured stdout as text (UTF-8; invalid bytes replaced), truncated to ~1 MiB
    on the cloud target (see :attr:`stdout_truncated`). This conversion is lossy for
    binary output — use :attr:`stdout_bytes` for byte-exact, untruncated output, or
    ``exec_stream`` to stream very large output."""
    stderr: str
    stdout_truncated: bool = False
    """True when the cloud capped the text :attr:`stdout` (1 MiB). :attr:`stdout_bytes`
    is still complete; or fetch big output via ``exec_stream`` / ``read_file``. Always
    False on the local target (the embedded engine streams unbounded)."""
    stderr_truncated: bool = False
    """True when the cloud capped the text :attr:`stderr` (1 MiB); see :attr:`stdout_truncated`."""
    stdout_bytes: bytes = b""
    """Byte-exact, untruncated stdout. Populated from the cloud's base64 output when
    the control provides it (older controls fall back to the UTF-8 bytes of the lossy
    :attr:`stdout`); on the local target it is the UTF-8 encoding of :attr:`stdout`.
    Prefer this over :attr:`stdout` for binary or >1 MiB output."""
    stderr_bytes: bytes = b""
    """Byte-exact, untruncated stderr; see :attr:`stdout_bytes`."""

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
class MachineUsageReport:
    """Per-machine usage + cost report (cloud target). Returned by
    :meth:`Machine.usage` and ``Machine.delete(include_usage=True)``; usage
    records survive deletion for 30 days.

    ``usage`` carries the metered totals (``totalUptimeSeconds``, ``cpuHours``,
    ``memoryGbHours``, ``diskGbHours``, ``egressGb``) and ``cost`` the
    micro-dollar breakdown (``cpuMicros`` … ``totalMicros``,
    ``amountDueMicros``), both keyed exactly as the API returns them."""

    machine_id: str
    from_ts: str
    """Report window start (RFC 3339) — the current billing period."""
    to_ts: str
    usage: dict[str, float]
    cost: dict[str, int]


@dataclass
class PortableCheckpointInfo:
    """Portable live machine checkpoint stored locally or by the cloud."""

    id: str
    machine_id: str
    status: str
    size_bytes: int
    arch: str
    created_at: str
    download_url: str
    path: Optional[str] = None
    source_pause_ms: Optional[float] = None
    elapsed_ms: Optional[float] = None


@dataclass
class ConnectOptions:
    """Selects and configures the backend. Local (embedded) is the default."""

    target: Optional[Literal["local", "cloud"]] = None
    base_url: Optional[str] = None
    api_key: Optional[str] = None


@dataclass
class PortEndpoint:
    """A way to reach a PUBLISHED guest port. Local endpoints use the current
    localhost host-port mapping; cloud endpoints use the authenticated bridge."""

    http_url: str
    """``https://…/v1/machines/:id/connect/:port[/path]`` — for HTTP requests."""
    ws_url: str
    """``wss://…/v1/machines/:id/connect/:port[/path]`` — for WebSocket upgrades."""
    headers: dict
    """Headers to send (the tenant Bearer token)."""
