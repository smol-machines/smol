"""NeMo Gym sandbox provider backed by local SmolVM or Smol Cloud.

Install ``smolmachines[nemo-gym]`` and select ``smol`` in NeMo Gym's sandbox
configuration.  A normal image creates a fresh machine; an image mapped in
``checkpoints`` forks a prepared, running machine instead, preserving its warm
process and filesystem state through copy-on-write cloning.
"""

from __future__ import annotations

import asyncio
import contextlib
import ipaddress
import math
import socket
import uuid
import weakref
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from nemo_gym.sandbox.providers.base import (
    SandboxCreateError,
    SandboxCreateVerificationError,
    SandboxEndpoint,
    SandboxExecResult,
    SandboxHandle,
    SandboxResources,
    SandboxSpec,
    SandboxStatus,
)

from .async_machine import AsyncEpisode, AsyncMachine
from .errors import SmolError
from .types import ConnectOptions, ExecOptions, MachineConfig, PortSpec, ResourceSpec

_RUNTIME_RETURN_CODE = 125
_PROBE_TEXT = "smol-sandbox-ready"
_TTL_DELETE_RETRIES = 3
_ALLOWED_PROVIDER_OPTIONS = {
    "allow_cidrs",
    "allow_hosts",
    "network_policy",
    "skip_health_check",
}


@dataclass(frozen=True)
class _Checkpoint:
    machine: str
    entrypoint: tuple[str, ...] | None = None
    ports: tuple[int, ...] = ()
    resources: SandboxResources | None = None
    network: tuple[bool, tuple[str, ...], tuple[str, ...]] = (True, (), ())


@dataclass
class _SmolSandbox:
    machine: AsyncMachine
    env: dict[str, str]
    workdir: str | None
    ports: tuple[int, ...]
    episode: AsyncEpisode | None = None
    ttl_task: asyncio.Task[None] | None = None
    closed: bool = False


@dataclass
class _PendingFork:
    name: str
    future: asyncio.Future[AsyncMachine]


class _ForkBatcher:
    """Coalesce independent NeMo create calls into one SDK fork-batch call."""

    def __init__(
        self,
        machine: str,
        connect: ConnectOptions,
        window_s: float,
        max_size: int,
    ) -> None:
        self._machine = machine
        self._connect = connect
        self._window_s = window_s
        self._max_size = max_size
        self._pending: list[_PendingFork] = []
        self._wake = asyncio.Event()
        self._task: asyncio.Task[None] | None = None

    async def submit(self, name: str) -> AsyncMachine:
        future = asyncio.get_running_loop().create_future()
        self._pending.append(_PendingFork(name=name, future=future))
        if self._task is None:
            self._wake.clear()
            self._task = asyncio.create_task(self._drain())
        if len(self._pending) >= self._max_size:
            self._wake.set()
        return await future

    async def _drain(self) -> None:
        try:
            if self._window_s > 0 and len(self._pending) < self._max_size:
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(self._wake.wait(), timeout=self._window_s)
            batch = self._pending[: self._max_size]
            del self._pending[: self._max_size]
            active = [item for item in batch if not item.future.cancelled()]
            if active:
                try:
                    golden = await AsyncMachine.connect(self._machine, self._connect)
                    clones = await golden.fork_batch(
                        names=[item.name for item in active]
                    )
                    if len(clones) != len(active):
                        await asyncio.gather(
                            *(clone.delete() for clone in clones),
                            return_exceptions=True,
                        )
                        raise RuntimeError(
                            "smol fork_batch returned "
                            f"{len(clones)} clones for {len(active)} requested names"
                        )
                    for item, clone in zip(active, clones):  # noqa: B905 - Python 3.9
                        if item.future.cancelled():
                            await clone.delete()
                        else:
                            item.future.set_result(clone)
                except BaseException as error:
                    for item in active:
                        if not item.future.done():
                            item.future.set_exception(error)
        finally:
            self._task = None
            if self._pending:
                self._wake.clear()
                self._task = asyncio.create_task(self._drain())


_BATCHERS: weakref.WeakKeyDictionary[
    asyncio.AbstractEventLoop, dict[tuple[Any, ...], _ForkBatcher]
] = weakref.WeakKeyDictionary()


def _fork_batcher(
    machine: str,
    connect: ConnectOptions,
    *,
    window_s: float,
    max_size: int,
) -> _ForkBatcher:
    loop = asyncio.get_running_loop()
    batchers = _BATCHERS.setdefault(loop, {})
    key = (
        machine,
        connect.target,
        connect.base_url,
        connect.api_key,
        window_s,
        max_size,
    )
    batcher = batchers.get(key)
    if batcher is None:
        batcher = _ForkBatcher(machine, connect, window_s, max_size)
        batchers[key] = batcher
    return batcher


def _checkpoint(value: str | Mapping[str, Any]) -> _Checkpoint:
    if isinstance(value, str):
        if not value:
            raise ValueError("checkpoint machine must be non-empty")
        return _Checkpoint(machine=value)
    if not isinstance(value, Mapping):
        raise TypeError("checkpoint values must be machine ids/names or mappings")
    unknown = set(value) - {
        "machine",
        "entrypoint",
        "ports",
        "resources",
        "provider_options",
    }
    if unknown:
        raise ValueError(f"Unknown checkpoint keys: {', '.join(sorted(unknown))}")
    machine = value.get("machine")
    if not isinstance(machine, str) or not machine:
        raise ValueError("checkpoint.machine must be a non-empty string")
    entrypoint_value = value.get("entrypoint")
    entrypoint: tuple[str, ...] | None = None
    if entrypoint_value is not None:
        if not isinstance(entrypoint_value, (list, tuple)) or not all(
            isinstance(arg, str) and arg for arg in entrypoint_value
        ):
            raise TypeError("checkpoint.entrypoint must be a list of non-empty strings")
        entrypoint = tuple(entrypoint_value)
    raw_ports = value.get("ports") or ()
    if not isinstance(raw_ports, (list, tuple)):
        raise TypeError("checkpoint.ports must be a list of TCP ports")
    if any(
        isinstance(port, bool)
        or not isinstance(port, (int, str))
        or (isinstance(port, str) and not port.isdigit())
        for port in raw_ports
    ):
        raise ValueError("checkpoint.ports must contain integer TCP ports")
    ports = tuple(int(port) for port in raw_ports)
    if any(port < 1 or port > 65535 for port in ports) or len(set(ports)) != len(ports):
        raise ValueError(
            "checkpoint.ports must contain unique ports between 1 and 65535"
        )
    raw_resources = value.get("resources")
    if raw_resources is not None and not isinstance(raw_resources, Mapping):
        raise TypeError("checkpoint.resources must be a mapping")
    resources = (
        SandboxResources.from_mapping(raw_resources)
        if raw_resources is not None
        else None
    )
    raw_options = value.get("provider_options") or {}
    if not isinstance(raw_options, Mapping):
        raise TypeError("checkpoint.provider_options must be a mapping")
    unknown_options = set(raw_options) - {
        "allow_cidrs",
        "allow_hosts",
        "network_policy",
    }
    if unknown_options:
        raise ValueError(
            "Unknown checkpoint provider option(s): "
            + ", ".join(sorted(unknown_options))
        )
    return _Checkpoint(
        machine=machine,
        entrypoint=entrypoint,
        ports=ports,
        resources=resources,
        network=_network_options(raw_options),
    )


def _host_from_target(target: str) -> str:
    parsed = urlsplit(target if "://" in target else f"//{target}")
    host = parsed.hostname
    if not host or any(char.isspace() for char in host):
        raise ValueError(f"Unsupported network policy target: {target!r}")
    host = host.removeprefix("*.").rstrip(".").lower()
    if not host:
        raise ValueError(f"Unsupported network policy target: {target!r}")
    return host


def _network_options(
    options: Mapping[str, Any],
) -> tuple[bool, tuple[str, ...], tuple[str, ...]]:
    raw_hosts = options.get("allow_hosts", [])
    raw_cidrs = options.get("allow_cidrs", [])
    if not isinstance(raw_hosts, (list, tuple)):
        raise TypeError("allow_hosts must be a list of hostnames")
    if not isinstance(raw_cidrs, (list, tuple)):
        raise TypeError("allow_cidrs must be a list of CIDRs")
    if any(not isinstance(value, str) or not value.strip() for value in raw_hosts):
        raise ValueError("allow_hosts must contain non-empty hostnames")
    if any(not isinstance(value, str) or not value.strip() for value in raw_cidrs):
        raise ValueError("allow_cidrs must contain non-empty CIDRs")
    hosts = [_host_from_target(str(value)) for value in raw_hosts]
    cidrs: list[str] = []
    for value in raw_cidrs:
        try:
            cidrs.append(str(ipaddress.ip_network(str(value), strict=False)))
        except ValueError as error:
            raise ValueError(f"Invalid allowed CIDR: {value!r}") from error
    policy = options.get("network_policy")
    if policy is None:
        return True, tuple(sorted(set(hosts))), tuple(sorted(set(cidrs)))
    if not isinstance(policy, Mapping):
        raise TypeError("provider_options.network_policy must be a mapping")
    default = str(policy.get("defaultAction", "deny")).lower()
    if default not in {"allow", "deny"}:
        raise ValueError("network_policy.defaultAction must be 'allow' or 'deny'")
    for rule in policy.get("egress", []):
        if not isinstance(rule, Mapping):
            raise TypeError("network_policy.egress entries must be mappings")
        action = str(rule.get("action", "allow")).lower()
        target = rule.get("target")
        if action == "deny" and default == "allow":
            raise ValueError(
                "smol cannot express deny-list exceptions to an allow-by-default policy"
            )
        if action != "allow" or target is None:
            continue
        target_text = str(target)
        try:
            cidrs.append(str(ipaddress.ip_network(target_text, strict=False)))
        except ValueError:
            host = _host_from_target(target_text)
            try:
                address = ipaddress.ip_address(host)
                cidrs.append(f"{address}/{address.max_prefixlen}")
            except ValueError:
                hosts.append(host)
    if default == "allow":
        return True, (), ()
    return (
        bool(hosts or cidrs),
        tuple(sorted(set(hosts))),
        tuple(sorted(set(cidrs))),
    )


def _network(spec: SandboxSpec) -> tuple[bool, list[str] | None, list[str] | None]:
    enabled, hosts, cidrs = _network_options(spec.provider_options)
    return enabled, list(hosts) or None, list(cidrs) or None


def _local_ports(ports: tuple[int, ...]) -> list[PortSpec]:
    """Reserve ephemeral host ports for the local embedded transport.

    The embedded port API currently requires an explicit host port. The socket
    is released immediately before VM creation, so this has the conventional
    small bind race; cloud allocation remains atomic in the control plane.
    """
    result: list[PortSpec] = []
    for guest in ports:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            host = int(sock.getsockname()[1])
        result.append(PortSpec(host=host, guest=guest))
    return result


class SmolProvider:
    """NeMo Gym sandbox provider using the public backend-agnostic smol SDK.

    ``target`` selects embedded local SmolVM or Smol Cloud. ``checkpoints`` maps
    an exact OCI image reference to a running forkable machine id/name (or a
    mapping with ``machine``, optional ``entrypoint``, and declared ``ports``).
    Exact matching prevents a task from accidentally running in the wrong base.
    """

    name = "smol"

    def __init__(
        self,
        *,
        target: str = "local",
        base_url: str | None = None,
        api_key: str | None = None,
        default_image: str | None = None,
        checkpoints: Mapping[str, str | Mapping[str, Any]] | None = None,
        gpu_mode: str = "cuda",
        probe_command: str | None = f"printf {_PROBE_TEXT}",
        probe_expected_stdout: str | None = _PROBE_TEXT,
        probe_timeout_s: float = 30.0,
        fork_batch_window_ms: float = 2.0,
        fork_batch_size: int = 32,
    ) -> None:
        if target not in {"local", "cloud"}:
            raise ValueError("target must be 'local' or 'cloud'")
        if gpu_mode not in {"cuda", "vulkan"}:
            raise ValueError("gpu_mode must be 'cuda' or 'vulkan'")
        if probe_timeout_s <= 0:
            raise ValueError("probe_timeout_s must be > 0")
        if not math.isfinite(probe_timeout_s):
            raise ValueError("probe_timeout_s must be finite")
        if fork_batch_window_ms < 0 or not math.isfinite(fork_batch_window_ms):
            raise ValueError("fork_batch_window_ms must be >= 0")
        if fork_batch_size < 1 or fork_batch_size > 256:
            raise ValueError("fork_batch_size must be between 1 and 256")
        self._target = target
        self._connect = ConnectOptions(
            target=target, base_url=base_url, api_key=api_key
        )
        self._default_image = default_image
        self._checkpoints = {
            str(image): _checkpoint(value)
            for image, value in (checkpoints or {}).items()
        }
        self._gpu_mode = gpu_mode
        self._probe_command = probe_command
        self._probe_expected_stdout = probe_expected_stdout
        self._probe_timeout_s = probe_timeout_s
        self._fork_batch_window_s = fork_batch_window_ms / 1000.0
        self._fork_batch_size = fork_batch_size
        self._sandboxes: dict[str, _SmolSandbox] = {}

    def _options(self, spec: SandboxSpec) -> Mapping[str, Any]:
        if not isinstance(spec.provider_options, Mapping):
            raise TypeError("smol provider_options must be a mapping")
        unknown = set(spec.provider_options) - _ALLOWED_PROVIDER_OPTIONS
        if unknown:
            raise ValueError(
                f"Unknown smol provider option(s): {', '.join(sorted(unknown))}. "
                f"Supported: {', '.join(sorted(_ALLOWED_PROVIDER_OPTIONS))}"
            )
        return spec.provider_options

    def _resource_spec(self, spec: SandboxSpec) -> ResourceSpec:
        resources = spec.resources
        for field in ("cpu", "memory_mib", "disk_gib"):
            value = getattr(resources, field)
            if value is not None and (value <= 0 or not math.isfinite(value)):
                raise ValueError(f"sandbox resource {field} must be finite and > 0")
        if resources.gpu_type is not None:
            raise ValueError(
                "smol does not yet support selecting a sandbox by gpu_type"
            )
        gpu_count = int(resources.gpu or 0)
        if gpu_count not in {0, 1}:
            raise ValueError(
                "smol currently supports at most one remoted GPU per sandbox"
            )
        if gpu_count and self._target == "cloud":
            raise ValueError(
                "NeMo Gym GPU placement is not yet exposed by the Smol Cloud SDK"
            )
        network, allow_hosts, allow_cidrs = _network(spec)
        return ResourceSpec(
            cpus=max(1, math.ceil(resources.cpu))
            if resources.cpu is not None
            else None,
            memory_mb=resources.memory_mib,
            storage_gb=resources.disk_gib,
            network=network,
            allow_hosts=allow_hosts,
            allow_cidrs=allow_cidrs,
            cuda=gpu_count == 1 and self._gpu_mode == "cuda",
            gpu=gpu_count == 1 and self._gpu_mode == "vulkan",
        )

    def _resolve_checkpoint(self, spec: SandboxSpec, image: str) -> _Checkpoint | None:
        checkpoint = self._checkpoints.get(image)
        if checkpoint is None:
            return None
        requested_entrypoint = tuple(spec.entrypoint) if spec.entrypoint else None
        if (
            requested_entrypoint is not None
            and requested_entrypoint != checkpoint.entrypoint
        ):
            raise ValueError(
                "a fork inherits the prepared checkpoint's running workload; "
                "SandboxSpec.entrypoint must be omitted or exactly match "
                "checkpoint.entrypoint"
            )
        missing_ports = set(spec.ports) - set(checkpoint.ports)
        if missing_ports:
            raise ValueError(
                "checkpoint does not declare requested port(s): "
                + ", ".join(str(port) for port in sorted(missing_ports))
            )
        self._validate_checkpoint_resources(checkpoint, spec.resources)
        requested_network = _network_options(spec.provider_options)
        if requested_network != checkpoint.network:
            raise ValueError(
                "a fork inherits its checkpoint's network policy; declare matching "
                "checkpoint.provider_options or use a cold sandbox"
            )
        return checkpoint

    @staticmethod
    def _validate_checkpoint_resources(
        checkpoint: _Checkpoint, requested: SandboxResources
    ) -> None:
        fields = ("cpu", "memory_mib", "disk_gib", "gpu", "gpu_type")
        if not any(getattr(requested, field) is not None for field in fields):
            return
        available = checkpoint.resources
        if available is None:
            raise ValueError(
                "a fork inherits its checkpoint's resources; declare "
                "checkpoint.resources or omit per-task resource requests"
            )
        for field in ("cpu", "memory_mib", "disk_gib", "gpu"):
            need = getattr(requested, field)
            if need is None or (field == "gpu" and need == 0):
                continue
            if need <= 0 or not math.isfinite(need):
                raise ValueError(f"sandbox resource {field} must be finite and > 0")
            capacity = getattr(available, field)
            if capacity is None or need > capacity:
                raise ValueError(
                    f"checkpoint resource {field}={capacity!r} cannot satisfy "
                    f"requested {field}={need!r}"
                )
        if requested.gpu_type is not None and requested.gpu_type != available.gpu_type:
            raise ValueError(
                f"checkpoint gpu_type={available.gpu_type!r} does not match "
                f"requested gpu_type={requested.gpu_type!r}"
            )

    async def create(self, spec: SandboxSpec) -> SandboxHandle:
        image = spec.image or self._default_image
        if not image:
            raise SandboxCreateError(
                "spec.image is required when no smol default_image is configured"
            )
        self._options(spec)
        timeout = 120.0 if spec.ready_timeout_s is None else float(spec.ready_timeout_s)
        if timeout <= 0 or not math.isfinite(timeout):
            raise SandboxCreateError("ready_timeout_s must be finite and > 0")
        ttl_s = None if spec.ttl_s is None else float(spec.ttl_s)
        if ttl_s is not None and (ttl_s <= 0 or not math.isfinite(ttl_s)):
            raise SandboxCreateError("ttl_s must be finite and > 0")
        name = f"nemo-gym-{uuid.uuid4().hex[:12]}"
        checkpoint = self._resolve_checkpoint(spec, image)
        machine: AsyncMachine | None = None
        episode: AsyncEpisode | None = None
        try:
            if checkpoint is not None:
                if self._target == "cloud" and ttl_s is not None:
                    golden = await AsyncMachine.connect(
                        checkpoint.machine, self._connect
                    )
                    episode = await golden.assign(
                        name,
                        ttl_secs=math.ceil(ttl_s),
                    )
                    machine = episode.machine
                else:
                    machine = await _fork_batcher(
                        checkpoint.machine,
                        self._connect,
                        window_s=self._fork_batch_window_s,
                        max_size=self._fork_batch_size,
                    ).submit(name)
            else:
                ports = (
                    _local_ports(tuple(spec.ports))
                    if self._target == "local"
                    else [PortSpec(host=port, guest=port) for port in spec.ports]
                )
                machine = await AsyncMachine.create(
                    MachineConfig(
                        name=name,
                        image=image,
                        command=list(spec.entrypoint) if spec.entrypoint else None,
                        ports=ports or None,
                        resources=self._resource_spec(spec),
                        ttl_seconds=math.ceil(ttl_s) if ttl_s is not None else None,
                        ready_timeout_seconds=timeout,
                        env=dict(spec.env),
                        workdir=spec.workdir,
                    ),
                    self._connect,
                )
            raw = _SmolSandbox(
                machine=machine,
                env=dict(spec.env),
                workdir=spec.workdir,
                ports=tuple(spec.ports),
                episode=episode,
            )
            handle = SandboxHandle(
                sandbox_id=machine.id, provider_name=self.name, raw=raw
            )
            if not self._options(spec).get("skip_health_check", False):
                await self._verify(handle)
            if ttl_s is not None and self._target == "local":
                raw.ttl_task = asyncio.create_task(self._expire(handle, ttl_s))
            self._sandboxes[handle.sandbox_id] = raw
            return handle
        except asyncio.CancelledError:
            await self._cleanup_partial(machine, episode)
            raise
        except Exception as error:
            await self._cleanup_partial(machine, episode)
            if isinstance(error, SandboxCreateError):
                raise
            raise SandboxCreateError(
                f"smol sandbox create failed for image={image!r}: {error}"
            ) from error

    @staticmethod
    async def _cleanup_partial(
        machine: AsyncMachine | None, episode: AsyncEpisode | None
    ) -> None:
        if episode is not None:
            try:
                await episode.complete("infra_failed")
                return
            except Exception:  # noqa: BLE001 - fall back to direct deletion
                pass
        if machine is not None:
            with contextlib.suppress(Exception):
                await machine.delete()

    async def _verify(self, handle: SandboxHandle) -> None:
        if self._probe_command is None:
            return
        result = await self.exec(
            handle, self._probe_command, timeout_s=self._probe_timeout_s
        )
        if result.return_code != 0 or (
            self._probe_expected_stdout is not None
            and self._probe_expected_stdout not in (result.stdout or "")
        ):
            detail = (
                f"return_code={result.return_code}, "
                f"stderr={(result.stderr or '').strip()!r}"
            )
            raise SandboxCreateVerificationError(
                f"smol sandbox {handle.sandbox_id!r} failed readiness probe: {detail}"
            )

    async def _expire(self, handle: SandboxHandle, ttl_s: float) -> None:
        try:
            await asyncio.sleep(max(0.0, ttl_s))
            for attempt in range(_TTL_DELETE_RETRIES):
                try:
                    await self.close(handle)
                    return
                except Exception:  # noqa: BLE001 - retry transient local teardown
                    if attempt + 1 == _TTL_DELETE_RETRIES:
                        return
                    await asyncio.sleep(0.1 * (2**attempt))
        except asyncio.CancelledError:
            raise

    async def exec(
        self,
        handle: SandboxHandle,
        command: str,
        *,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        timeout_s: int | float | None = None,
        user: str | int | None = None,
    ) -> SandboxExecResult:
        raw: _SmolSandbox = handle.raw
        if raw.closed:
            return SandboxExecResult(
                stdout=None,
                stderr="sandbox is closed",
                return_code=_RUNTIME_RETURN_CODE,
                error_type="sandbox",
            )
        if user not in {None, "root", 0, "0"}:
            # Smol's exec protocol currently starts as root. Use the guest's
            # standard su implementation so ordinary SWE images retain their
            # established per-task user without adding a provider-specific API.
            import shlex

            command = (
                f"exec su -s /bin/sh {shlex.quote(str(user))} -c {shlex.quote(command)}"
            )
        merged_env = dict(raw.env)
        if env:
            merged_env.update(env)
        timeout = max(1, math.ceil(timeout_s)) if timeout_s is not None else None
        try:
            result = await raw.machine.exec(
                ["/bin/sh", "-lc", command],
                ExecOptions(
                    env=merged_env or None, workdir=cwd or raw.workdir, timeout=timeout
                ),
            )
            return SandboxExecResult(
                stdout=result.stdout,
                stderr=result.stderr,
                return_code=result.exit_code,
                error_type=None,
            )
        except SmolError as error:
            error_type = "timeout" if error.code == "TIMEOUT" else "sandbox"
            return SandboxExecResult(
                stdout=None,
                stderr=str(error),
                return_code=_RUNTIME_RETURN_CODE,
                error_type=error_type,
            )
        except Exception as error:  # noqa: BLE001 - translate provider transport failures
            return SandboxExecResult(
                stdout=None,
                stderr=str(error),
                return_code=_RUNTIME_RETURN_CODE,
                error_type="sandbox",
            )

    async def upload_file(
        self, handle: SandboxHandle, source_path: Path, target_path: str
    ) -> None:
        parent = str(Path(target_path).parent)
        if parent not in {"", "."}:
            import shlex

            result = await self.exec(
                handle, f"mkdir -p {shlex.quote(parent)}", user="root"
            )
            if result.return_code != 0:
                raise RuntimeError(
                    f"smol upload mkdir failed for {target_path!r}: {result.stderr}"
                )
        mode = source_path.stat().st_mode & 0o777
        await handle.raw.machine.write_file(
            target_path, source_path.read_bytes(), mode=mode
        )

    async def download_file(
        self, handle: SandboxHandle, source_path: str, target_path: Path
    ) -> None:
        data = await handle.raw.machine.read_file(source_path)
        target_path.parent.mkdir(parents=True, exist_ok=True)
        target_path.write_bytes(data)

    async def status(self, handle: SandboxHandle) -> SandboxStatus:
        raw: _SmolSandbox = handle.raw
        if raw.closed:
            return SandboxStatus.STOPPED
        try:
            state = await raw.machine.state()
        except SmolError as error:
            return (
                SandboxStatus.STOPPED
                if error.code in {"NOT_FOUND", "VM_NOT_FOUND"}
                else SandboxStatus.UNKNOWN
            )
        except Exception:  # noqa: BLE001 - provider status must degrade to UNKNOWN
            return SandboxStatus.UNKNOWN
        if state in {"running", "started"}:
            return SandboxStatus.RUNNING
        if state in {"created", "starting"}:
            return SandboxStatus.STARTING
        if state in {"stopped", "deleted"}:
            return SandboxStatus.STOPPED
        if state in {"error", "failed"}:
            return SandboxStatus.ERROR
        return SandboxStatus.UNKNOWN

    async def serialize_handle(
        self, handle: SandboxHandle, *, scope: str | None = None
    ) -> dict[str, Any]:
        """Return a credential-free descriptor another provider can reconnect.

        ``scope`` is intentionally ignored: Smol Cloud authorization remains in
        the receiving provider's SDK connection, never in a trajectory payload.
        """
        del scope
        raw: _SmolSandbox = handle.raw
        return {
            "sandbox_id": handle.sandbox_id,
            "env": dict(raw.env),
            "workdir": raw.workdir,
            "ports": list(raw.ports),
        }

    async def connect(self, descriptor: Mapping[str, Any]) -> SandboxHandle:
        """Reconnect to a live local or cloud sandbox by its opaque machine id."""
        sandbox_id = descriptor.get("sandbox_id")
        if not isinstance(sandbox_id, str) or not sandbox_id:
            raise ValueError("smol sandbox descriptor requires a non-empty sandbox_id")
        env = descriptor.get("env") or {}
        if not isinstance(env, Mapping):
            raise TypeError("smol sandbox descriptor env must be a mapping")
        ports_value = descriptor.get("ports") or ()
        if not isinstance(ports_value, (list, tuple)):
            raise TypeError("smol sandbox descriptor ports must be a list")
        if any(isinstance(port, bool) for port in ports_value):
            raise ValueError("smol sandbox descriptor contains an invalid port")
        ports = tuple(int(port) for port in ports_value)
        if any(port < 1 or port > 65535 for port in ports) or len(set(ports)) != len(
            ports
        ):
            raise ValueError("smol sandbox descriptor contains an invalid port")
        machine = await AsyncMachine.connect(sandbox_id, self._connect)
        await machine.wait_until_ready()
        raw = _SmolSandbox(
            machine=machine,
            env={str(key): str(value) for key, value in env.items()},
            workdir=str(descriptor["workdir"])
            if descriptor.get("workdir") is not None
            else None,
            ports=ports,
        )
        self._sandboxes[sandbox_id] = raw
        return SandboxHandle(sandbox_id=sandbox_id, provider_name=self.name, raw=raw)

    async def endpoint(self, handle: SandboxHandle, port: int) -> SandboxEndpoint:
        raw: _SmolSandbox = handle.raw
        if port not in raw.ports:
            raise ValueError(
                f"Sandbox port {port} was not declared in SandboxSpec.ports; "
                f"declared ports: {list(raw.ports)!r}"
            )
        endpoint = raw.machine.endpoint(port)
        return SandboxEndpoint(
            endpoint=endpoint.http_url, headers=dict(endpoint.headers)
        )

    async def close(self, handle: SandboxHandle) -> None:
        raw: _SmolSandbox = handle.raw
        if raw.closed:
            return
        raw.closed = True
        task, raw.ttl_task = raw.ttl_task, None
        if task is not None and task is not asyncio.current_task():
            task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await task
        try:
            if raw.episode is not None:
                await raw.episode.complete("done")
            else:
                await raw.machine.delete()
        except SmolError as error:
            if error.code not in {"NOT_FOUND", "VM_NOT_FOUND"}:
                raw.closed = False
                raise
        except BaseException:
            raw.closed = False
            raise
        finally:
            if raw.closed:
                self._sandboxes.pop(handle.sandbox_id, None)

    async def aclose(self) -> None:
        # AsyncMachine owns no shared client/session. Sandbox lifecycle belongs
        # to close(handle); aclose must not tear down other live handles when a
        # provider instance is intentionally shared by concurrent episodes.
        return None


__all__ = ["SmolProvider"]
