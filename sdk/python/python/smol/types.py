"""Public types for the ``smol`` SDK — backend-agnostic, mirroring ``types.ts``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Literal, Optional

__all__ = [
    "ResourceSpec",
    "MountSpec",
    "PortSpec",
    "MachineConfig",
    "EgressInterceptor",
    "ExecOptions",
    "ExecResult",
    "ImageInfo",
    "ConnectOptions",
    "MachineState",
    "MachineUsageReport",
    "PortableCheckpointInfo",
    "MachineResources",
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
    """Enable unrestricted outbound network access (TSI). Default: False. To
    allow only specific destinations, set ``allow_hosts`` or ``allow_cidrs``
    instead of this flag; either one enables networking scoped to its list."""
    allow_cidrs: Optional[list[str]] = None
    """Scope egress to these CIDR ranges. Setting this (or allow_hosts) enables
    networking and restricts it to the listed CIDRs. Enforced on both the local
    and cloud targets."""
    allow_hosts: Optional[list[str]] = None
    """Scope egress to these hostnames and their subdomains (e.g.
    api.anthropic.com). Setting this (or allow_cidrs) enables networking and
    restricts it to the listed hosts; names are enforced at DNS inside the
    machine, so a host served from another domain (a CDN) needs its own entry.
    Enforced on both the local and cloud targets."""
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
class EgressInterceptor:
    """Per-launch trusted host egress service. Keep the token out of logs and files."""

    address: str
    token: str = field(repr=False)


@dataclass
class CredentialSpec:
    """A credential the workload uses without ever seeing it, like the CLI's
    ``--credential``. The guest variable ``env_var`` holds a placeholder; the
    real value is substituted only in the headers of HTTPS requests to
    ``hosts``, so no other destination can receive it."""

    name: str
    """Binding name: 1-64 lowercase letters, digits, ``-`` and ``_``. On the
    cloud it names the credential stored for your account."""
    env_var: str
    """Guest environment variable that holds the placeholder."""
    hosts: list[str]
    """Exact host names the value may be sent to. No wildcards: list each
    subdomain."""
    value: Optional[str] = field(default=None, repr=False)
    """The real value. Locally it is held in this process's memory only;
    when omitted, this process's own ``env_var`` is read at each start. On the
    cloud it is stored sealed under ``name``; when omitted, the credential
    already stored under ``name`` is used."""
    methods: Optional[list[str]] = None
    """HTTP methods the value may be used with, e.g. ``["GET", "HEAD"]`` for a
    read-only token. Every method when omitted (local)."""


@dataclass
class MachineConfig:
    """Configuration for creating a machine."""

    name: Optional[str] = None
    egress_interceptor: Optional[EgressInterceptor] = field(default=None, repr=False)
    """Machine name (auto-generated if omitted)."""
    image: Optional[str] = None
    """Base image. Required for the cloud target; optional for local."""
    labels: Optional[dict[str, str]] = None
    """Caller metadata for finding the machine again with :meth:`Machine.list`.
    Cloud only."""
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
    no existing config changes meaning. For an allowlist rather than open
    access, set ``resources.allow_hosts`` or ``resources.allow_cidrs``."""
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
    wait_for_ports: bool = True
    """Wait for published services at creation. Set false when installing a
    service through exec after boot; creation still verifies guest execution."""
    branchable: Optional[bool] = None
    """Start as a live-RAM branch source so the machine can produce independent
    copy-on-write children with :meth:`Machine.branch`."""
    forkable: bool = False
    """Deprecated alias for :attr:`branchable`."""
    checkpoint: bool = False
    """Deprecated alias for :attr:`branchable`; checkpoints are durable artifacts."""
    env: Optional[dict[str, str]] = None
    """Environment variables for the image workload launched at create."""
    credentials: Optional[list[CredentialSpec]] = None
    """Credentials the workload uses without seeing them. Implies network
    access. See :class:`CredentialSpec`."""
    cache_disk: Optional["CacheDiskRef"] = None
    """Start from a published version of one of your cache disks, mounted at
    its mount path (or ``mount_path``) through the machine's own copy-on-write
    layer. Cloud only. See :class:`smol.CacheDisk`."""
    workdir: Optional[str] = None
    """Working directory for the image workload, set at create. Overrides the
    image's own workdir."""
    user: Optional[str] = None
    """Run the workload as this user: a name from the image or a numeric
    ``uid[:gid]``. Overrides the image's USER, so a workload can match the owner
    of a mounted host directory."""

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
    timeout: Optional[float] = None
    """Kill the command after this many seconds. Fractions are honored: locally
    to the millisecond, on the cloud rounded up to a whole second. Must be > 0."""
    user: Optional[str] = None
    """Run the command as this user: a name from the image or a numeric
    ``uid[:gid]``. Image machines only — a bare VM runs every command as root, so
    asking for a user there is an error rather than a silent root. On the cloud
    the SDK first checks that the control plane applies it and raises
    :class:`NotSupportedError` before running anything if it would not. Not
    supported by local ``run(image, ...)``."""
    output: Optional[Literal["text", "b64", "both"]] = None
    """Cloud target only: which output encodings the server returns. The default
    carries both the capped UTF-8 text fields and the byte-exact base64 fields;
    ``"text"`` or ``"b64"`` halves the response payload by dropping the other
    family. With ``"text"``, :attr:`ExecResult.stdout_bytes` degrades to the
    lossy text re-encoded; with ``"b64"``, :attr:`ExecResult.stdout` is empty.
    Ignored (both families, as before) on older control planes and locally."""
    background: bool = False
    """Cloud only: start the command detached and return at once with its
    :attr:`ExecResult.pid`, leaving it running (a dev server, an agent)."""


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
    pid: Optional[int] = None
    """The detached process's pid, for an exec run with ``background=True``."""

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
class ShareLink:
    """An anonymous share link for a machine's published app (cloud target).

    Anyone holding ``url`` reaches the app without a smolmachines account, so
    treat it as a credential. ``url`` is ``None`` when the tenant has no apps
    domain configured or the machine name is not DNS-safe; attach ``token`` as
    ``?t=<token>`` in that case. Minting again replaces the previous token.
    """

    token: str
    url: Optional[str] = None


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
    reused_bytes: Optional[int] = None
    path: Optional[str] = None
    source_pause_ms: Optional[float] = None
    elapsed_ms: Optional[float] = None


@dataclass
class MachineResources:
    """A machine's resources after a resize."""

    cpus: int
    """Online vCPUs."""
    memory_mb: int
    """Memory in MB."""
    storage_gb: Optional[int] = None
    """Storage disk size in GB, or None while it has the default size."""
    overlay_gb: Optional[int] = None
    """Overlay disk size in GB, or None while it has the default size."""


@dataclass
class ConnectOptions:
    """Selects and configures the backend. Local (embedded) is the default."""

    target: Optional[Literal["local", "cloud"]] = None
    base_url: Optional[str] = None
    api_key: Optional[str] = None
    egress_interceptor: Optional[EgressInterceptor] = field(default=None, repr=False)


@dataclass
class MachineSummary:
    """One machine as :meth:`Machine.list` reports it."""

    id: str
    name: str
    state: str
    labels: dict[str, str]
    image: Optional[str] = None
    persistent: bool = True
    branchable: bool = False
    created_at: Optional[str] = None


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


@dataclass
class CacheDiskRef:
    """Which cache disk a machine starts from."""

    cache: str
    """The cache disk's id or name."""
    version: Optional[int] = None
    """Version to start from; the latest when ``None``."""
    mount_path: Optional[str] = None
    """Absolute guest path to mount it at; the cache disk's own when ``None``."""


@dataclass
class CacheDiskVersion:
    """One immutable version of a cache disk."""

    version: int
    """0 is the empty filesystem a cache disk starts as; each publish adds one."""
    size_bytes: int
    sha256: str
    created_at: str
    source_machine_id: Optional[str] = None
    """The machine this version was published from; ``None`` for v0."""


@dataclass
class CacheDiskInfo:
    """A cache disk: a disk image many machines start from, each through its
    own copy-on-write layer, published in immutable versions."""

    id: str
    name: str
    size_gb: int
    mount_path: str
    latest_version: int
    versions: list[CacheDiskVersion]
    """Newest first."""
    created_at: str


@dataclass
class PublishedCacheDisk:
    """What publishing a machine's cache disk created."""

    cache_disk: CacheDiskInfo
    version: CacheDiskVersion
