"""Harbor environment provider backed by local SmolVM or Smol Cloud.

Select it with ``harbor run ... -e smol.harbor:SmolEnvironment``.  By default,
the provider creates one warm, forkable machine for each distinct Harbor task
environment and copy-on-write forks a clean trial machine from it.  Set
``auto_checkpoint=false`` to create a cold machine for every trial, or provide
``checkpoints`` to fork an already-prepared machine.
"""

from __future__ import annotations

import asyncio
import atexit
import contextlib
import ipaddress
import math
import os
import re
import shlex
import tarfile
import tempfile
import uuid
import weakref
from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Literal, override

from filelock import AsyncFileLock
from harbor.environments.base import BaseEnvironment, ExecResult
from harbor.environments.capabilities import (
    EnvironmentCapabilities,
    EnvironmentResourceCapabilities,
)
from harbor.models.task.config import (
    EnvironmentConfig,
    NetworkAllowlistEntryType,
    NetworkMode,
    classify_network_allowlist_entry,
)
from harbor.models.trial.paths import TrialPaths

from .async_machine import AsyncMachine
from .errors import SmolError
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

_SHELL_EXEC = (
    "if [ -x /bin/bash ]; then shell=/bin/bash; else shell=/bin/sh; fi; "
    'exec "$shell" -lc "$1"'
)
_SHELL_USER_EXEC = (
    "if [ -x /bin/bash ]; then shell=/bin/bash; else shell=/bin/sh; fi; "
    'exec su -s "$shell" "$2" -c "$1"'
)
_TRANSFER_DIR = PurePosixPath("/tmp")
_DOCKER_OUTPUT_LIMIT = 8_000


def _image_cache_root() -> Path:
    override = os.environ.get("SMOL_HARBOR_IMAGE_CACHE")
    if override:
        return Path(override).expanduser()
    cache_home = os.environ.get("XDG_CACHE_HOME")
    root = Path(cache_home).expanduser() if cache_home else Path.home() / ".cache"
    return root / "smol" / "harbor-images"


def _docker_error_output(stdout: bytes, stderr: bytes) -> str:
    output = (stdout + stderr).decode(errors="replace").strip()
    if len(output) > _DOCKER_OUTPUT_LIMIT:
        output = output[-_DOCKER_OUTPUT_LIMIT:]
    return output


async def _docker_output(*args: str, timeout_sec: float = 30.0) -> str:
    try:
        process = await asyncio.create_subprocess_exec(
            "docker",
            *args,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except FileNotFoundError as error:
        raise RuntimeError(
            "Dockerfile-backed Smol environments require the docker CLI and a "
            "reachable Docker daemon"
        ) from error
    try:
        stdout, stderr = await asyncio.wait_for(
            process.communicate(), timeout=timeout_sec
        )
    except TimeoutError as error:
        process.kill()
        await process.wait()
        raise RuntimeError(f"docker {' '.join(args)} timed out") from error
    if process.returncode != 0:
        detail = _docker_error_output(stdout, stderr)
        suffix = f": {detail}" if detail else ""
        raise RuntimeError(f"docker {' '.join(args)} failed{suffix}")
    return stdout.decode(errors="replace").strip()


async def _docker_image_id(image: str) -> str | None:
    try:
        return await _docker_output(
            "image", "inspect", "--format={{.Id}}", image, timeout_sec=30
        )
    except RuntimeError as error:
        # A missing local tag is the normal cache-miss case. Other failures are
        # surfaced by the following build, with its complete build log path.
        if "No such image" in str(error) or "No such object" in str(error):
            return None
        raise


async def _run_docker_logged(
    *args: str,
    log_path: Path,
    timeout_sec: float,
) -> None:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with log_path.open("wb") as log:
            process = await asyncio.create_subprocess_exec(
                "docker",
                *args,
                stdin=asyncio.subprocess.DEVNULL,
                stdout=log,
                stderr=asyncio.subprocess.STDOUT,
            )
            try:
                await asyncio.wait_for(process.wait(), timeout=timeout_sec)
            except TimeoutError as error:
                process.kill()
                await process.wait()
                raise RuntimeError(
                    f"docker {' '.join(args)} timed out after {timeout_sec:g}s; "
                    f"see {log_path}"
                ) from error
    except FileNotFoundError as error:
        raise RuntimeError(
            "Dockerfile-backed Smol environments require the docker CLI and a "
            "reachable Docker daemon"
        ) from error
    if process.returncode != 0:
        detail = log_path.read_text(errors="replace")[-_DOCKER_OUTPUT_LIMIT:].strip()
        suffix = f": {detail}" if detail else ""
        raise RuntimeError(f"docker {' '.join(args)} failed; see {log_path}{suffix}")


async def _prepare_local_dockerfile_image(
    *,
    environment_dir: Path,
    environment_id: str,
    force_build: bool,
    timeout_sec: float,
) -> str:
    """Build and atomically export a Dockerfile as a SmolVM image archive."""

    platform = await _docker_output(
        "version", "--format={{.Server.Os}}/{{.Server.Arch}}", timeout_sec=30
    )
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", platform):
        raise RuntimeError(f"docker returned an invalid server platform: {platform!r}")
    platform_key = platform.replace("/", "-").lower()
    tag = f"smol-harbor:{environment_id}-{platform_key}"
    cache_dir = _image_cache_root() / environment_id / platform_key
    cache_dir.mkdir(parents=True, exist_ok=True)
    lock = AsyncFileLock(cache_dir / ".build.lock")

    async with lock:
        image_id = await _docker_image_id(tag)
        if force_build or image_id is None:
            await _run_docker_logged(
                "build",
                f"--platform={platform}",
                f"--file={environment_dir / 'Dockerfile'}",
                f"--tag={tag}",
                str(environment_dir),
                log_path=cache_dir / "build.log",
                timeout_sec=timeout_sec,
            )
            image_id = await _docker_image_id(tag)
            if image_id is None:
                raise RuntimeError(f"docker build completed without creating {tag}")

        image_key = re.sub(r"[^A-Za-z0-9_.-]+", "-", image_id).strip("-")
        archive = cache_dir / f"{image_key}.tar"
        if (
            archive.is_file()
            and archive.stat().st_size > 0
            and tarfile.is_tarfile(archive)
        ):
            return str(archive.resolve())

        temporary = cache_dir / f".{image_key}.{uuid.uuid4().hex}.tmp"
        try:
            await _run_docker_logged(
                "image",
                "save",
                f"--output={temporary}",
                tag,
                log_path=cache_dir / "export.log",
                timeout_sec=timeout_sec,
            )
            if not temporary.is_file() or temporary.stat().st_size == 0:
                raise RuntimeError(
                    f"docker image save created an empty archive: {temporary}"
                )
            if not tarfile.is_tarfile(temporary):
                raise RuntimeError(
                    f"docker image save created an invalid archive: {temporary}"
                )
            temporary.replace(archive)
        finally:
            temporary.unlink(missing_ok=True)
        return str(archive.resolve())


def _machine_name(value: str) -> str:
    normalized = re.sub(r"[^A-Za-z0-9-]+", "-", value).strip("-").lower()
    return (normalized or "harbor")[:128].rstrip("-")


@dataclass(frozen=True)
class _Checkpoint:
    machine: str
    resources: Mapping[str, int] | None = None
    network_mode: str | None = None
    allowed_hosts: tuple[str, ...] = ()


def _checkpoint(value: str | Mapping[str, Any]) -> _Checkpoint:
    if isinstance(value, str):
        if not value:
            raise ValueError("checkpoint machine must be non-empty")
        return _Checkpoint(machine=value)
    if not isinstance(value, Mapping):
        raise TypeError("checkpoint values must be machine ids/names or mappings")
    unknown = set(value) - {"machine", "resources", "network_mode", "allowed_hosts"}
    if unknown:
        raise ValueError(f"Unknown checkpoint keys: {', '.join(sorted(unknown))}")
    machine = value.get("machine")
    if not isinstance(machine, str) or not machine:
        raise ValueError("checkpoint.machine must be a non-empty string")
    resources = value.get("resources")
    if resources is not None:
        if not isinstance(resources, Mapping):
            raise TypeError("checkpoint.resources must be a mapping")
        unknown_resources = set(resources) - {
            "cpus",
            "memory_mb",
            "storage_mb",
            "gpus",
        }
        if unknown_resources:
            raise ValueError(
                "Unknown checkpoint resource keys: "
                + ", ".join(sorted(unknown_resources))
            )
        normalized_resources: dict[str, int] = {}
        for name, raw in resources.items():
            if isinstance(raw, bool) or not isinstance(raw, int) or raw < 0:
                raise ValueError(f"checkpoint resource {name} must be an integer >= 0")
            normalized_resources[str(name)] = raw
        resources = normalized_resources
    network_mode = value.get("network_mode")
    if network_mode is not None and network_mode not in {
        NetworkMode.PUBLIC.value,
        NetworkMode.NO_NETWORK.value,
        NetworkMode.ALLOWLIST.value,
    }:
        raise ValueError("checkpoint.network_mode is invalid")
    allowed_hosts = value.get("allowed_hosts", ())
    if not isinstance(allowed_hosts, (list, tuple)) or not all(
        isinstance(host, str) and host for host in allowed_hosts
    ):
        raise TypeError("checkpoint.allowed_hosts must be a list of strings")
    return _Checkpoint(
        machine=machine,
        resources=resources,
        network_mode=network_mode,
        allowed_hosts=tuple(allowed_hosts),
    )


@dataclass
class _PendingFork:
    name: str
    future: asyncio.Future[AsyncMachine]


class _ForkBatcher:
    """Coalesce concurrent Harbor starts into a single Smol fork-batch call."""

    def __init__(
        self,
        resolve_golden: Callable[[], Awaitable[AsyncMachine]],
        *,
        window_s: float,
        max_size: int,
    ) -> None:
        self._resolve_golden = resolve_golden
        self._window_s = window_s
        self._max_size = max_size
        self._pending: list[_PendingFork] = []
        self._wake = asyncio.Event()
        self._task: asyncio.Task[None] | None = None

    async def submit(self, name: str) -> AsyncMachine:
        future: asyncio.Future[AsyncMachine] = (
            asyncio.get_running_loop().create_future()
        )
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
                    golden = await self._resolve_golden()
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
                            f"{len(clones)} clones for {len(active)} requests"
                        )
                    for item, clone in zip(active, clones):
                        if item.future.cancelled():
                            await clone.delete()
                        else:
                            item.future.set_result(clone)
                except BaseException as error:  # noqa: BLE001 - wake waiters on cancellation
                    for item in active:
                        if not item.future.done():
                            item.future.set_exception(error)
        finally:
            self._task = None
            if self._pending:
                self._wake.clear()
                self._task = asyncio.create_task(self._drain())


@dataclass
class _LoopState:
    golden_tasks: dict[tuple[Any, ...], asyncio.Task[AsyncMachine]]
    batchers: dict[tuple[Any, ...], _ForkBatcher]


_LOOP_STATES: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, _LoopState] = (
    weakref.WeakKeyDictionary()
)
_OWNED_GOLDENS: list[AsyncMachine] = []


def _loop_state() -> _LoopState:
    loop = asyncio.get_running_loop()
    state = _LOOP_STATES.get(loop)
    if state is None:
        state = _LoopState(golden_tasks={}, batchers={})
        _LOOP_STATES[loop] = state
    return state


def _cleanup_owned_goldens() -> None:
    # AsyncMachine is a thin wrapper around the package's sync Machine.  At
    # interpreter shutdown there may be no event loop or executor left, so use
    # that owned sync handle directly rather than trying to schedule a coroutine.
    while _OWNED_GOLDENS:
        machine = _OWNED_GOLDENS.pop()
        with contextlib.suppress(Exception):
            machine._m.delete()


atexit.register(_cleanup_owned_goldens)


async def close_harbor_goldens() -> None:
    """Delete auto-created Harbor checkpoint machines before process shutdown."""

    machines = list(_OWNED_GOLDENS)
    _OWNED_GOLDENS.clear()
    if machines:
        await asyncio.gather(
            *(machine.delete() for machine in machines), return_exceptions=True
        )
    _LOOP_STATES.clear()


class SmolEnvironment(BaseEnvironment):
    """Run Harbor trials in copy-on-write SmolVM forks."""

    def __init__(
        self,
        environment_dir: Path,
        environment_name: str,
        session_id: str,
        trial_paths: TrialPaths,
        task_env_config: EnvironmentConfig,
        *args: Any,
        target: Literal["local", "cloud"] = "local",
        api_key: str | None = None,
        base_url: str | None = None,
        auto_checkpoint: bool = True,
        checkpoints: Mapping[str, str | Mapping[str, Any]] | None = None,
        fork_batch_window_ms: float = 2.0,
        fork_batch_size: int = 32,
        gpu_mode: Literal["cuda", "vulkan"] = "cuda",
        ready_timeout_sec: float = 120.0,
        **kwargs: Any,
    ) -> None:
        if target not in {"local", "cloud"}:
            raise ValueError("target must be 'local' or 'cloud'")
        if gpu_mode not in {"cuda", "vulkan"}:
            raise ValueError("gpu_mode must be 'cuda' or 'vulkan'")
        if not math.isfinite(fork_batch_window_ms) or fork_batch_window_ms < 0:
            raise ValueError("fork_batch_window_ms must be finite and >= 0")
        if not 1 <= fork_batch_size <= 256:
            raise ValueError("fork_batch_size must be between 1 and 256")
        if not math.isfinite(ready_timeout_sec) or ready_timeout_sec <= 0:
            raise ValueError("ready_timeout_sec must be finite and > 0")
        self._target = target
        self._connect = ConnectOptions(
            target=target,
            api_key=api_key,
            base_url=base_url,
        )
        self._auto_checkpoint = auto_checkpoint
        self._checkpoints = {
            str(key): _checkpoint(value) for key, value in (checkpoints or {}).items()
        }
        self._fork_batch_window_s = fork_batch_window_ms / 1000.0
        self._fork_batch_size = fork_batch_size
        self._gpu_mode = gpu_mode
        self._ready_timeout_sec = ready_timeout_sec
        self._machine: AsyncMachine | None = None
        self._resolved_image = task_env_config.docker_image
        super().__init__(
            environment_dir,
            environment_name,
            session_id,
            trial_paths,
            task_env_config,
            *args,
            **kwargs,
        )

    @staticmethod
    @override
    def type() -> str:
        return "smol"

    @property
    @override
    def capabilities(self) -> EnvironmentCapabilities:
        return EnvironmentCapabilities(
            gpus=self._target == "local",
            disable_internet=True,
            network_allowlist=True,
            network_allowlist_hostnames=True,
            network_allowlist_ipv4_addresses=True,
            network_allowlist_ipv6_addresses=True,
            network_allowlist_ipv4_cidrs=True,
            network_allowlist_ipv6_cidrs=True,
            mounted=False,
        )

    @classmethod
    @override
    def resource_capabilities(cls) -> EnvironmentResourceCapabilities:
        return EnvironmentResourceCapabilities(cpu_limit=True, memory_limit=True)

    @override
    def _validate_definition(self) -> None:
        if self.os.value != "linux":
            raise ValueError("SmolEnvironment currently supports Linux tasks only")
        if any(
            (self.environment_dir / name).exists()
            for name in ("docker-compose.yaml", "docker-compose.yml")
        ):
            raise ValueError(
                "SmolEnvironment does not yet support Docker Compose tasks"
            )
        dockerfile = self.environment_dir / "Dockerfile"
        if not self.task_env_config.docker_image and not dockerfile.is_file():
            raise ValueError(
                "SmolEnvironment requires [environment].docker_image or an "
                "environment/Dockerfile"
            )
        if not self.task_env_config.docker_image and self._target == "cloud":
            raise ValueError(
                "Smol Cloud cannot use a local environment/Dockerfile yet; publish "
                "the image and set [environment].docker_image"
            )

    async def _prepare_image(self, *, force_build: bool) -> None:
        configured = self.task_env_config.docker_image
        if configured is not None and not force_build:
            self._resolved_image = configured
            return
        dockerfile = self.environment_dir / "Dockerfile"
        if not dockerfile.is_file():
            raise ValueError(
                "force_build requires an environment/Dockerfile for SmolEnvironment"
            )
        if self._target != "local":
            raise ValueError(
                "Smol Cloud cannot build a local environment/Dockerfile yet; publish "
                "the image and set [environment].docker_image"
            )
        self._resolved_image = await _prepare_local_dockerfile_image(
            environment_dir=self.environment_dir,
            environment_id=self.environment_id,
            force_build=force_build,
            timeout_sec=self.task_env_config.build_timeout_sec,
        )

    def _image(self) -> str:
        if self._resolved_image is None:
            raise RuntimeError("SmolEnvironment image has not been prepared")
        return self._resolved_image

    def _resource_spec(self) -> ResourceSpec:
        gpus = self._effective_gpus
        if gpus not in {0, 1}:
            raise ValueError("SmolEnvironment currently supports at most one GPU")
        if gpus and self._target == "cloud":
            raise ValueError("Smol Cloud GPU placement is not exposed by the SDK yet")
        network, allow_hosts, allow_cidrs = self._smol_network()
        storage_mb = self._effective_storage_mb
        return ResourceSpec(
            cpus=self._effective_cpus,
            memory_mb=self._effective_memory_mb,
            storage_gb=math.ceil(storage_mb / 1024) if storage_mb else None,
            network=network,
            allow_hosts=allow_hosts or None,
            allow_cidrs=allow_cidrs or None,
            cuda=gpus == 1 and self._gpu_mode == "cuda",
            gpu=gpus == 1 and self._gpu_mode == "vulkan",
        )

    def _smol_network(self) -> tuple[bool, list[str], list[str]]:
        policy = self.network_policy
        if policy.network_mode == NetworkMode.PUBLIC:
            return True, [], []
        if policy.network_mode == NetworkMode.NO_NETWORK:
            return False, [], []
        hosts: list[str] = []
        cidrs: list[str] = []
        for entry in policy.allowed_hosts:
            entry_type = classify_network_allowlist_entry(entry)
            if entry_type == NetworkAllowlistEntryType.HOSTNAME:
                hosts.append(entry)
            elif entry_type == NetworkAllowlistEntryType.WILDCARD_HOSTNAME:
                # The capability validator rejects wildcard entries. Keep this
                # guard in case a future Harbor version bypasses that validation.
                raise ValueError("SmolEnvironment does not support wildcard hostnames")
            else:
                cidrs.append(str(ipaddress.ip_network(entry, strict=False)))
        return True, hosts, cidrs

    def _resolve_checkpoint(self) -> _Checkpoint | None:
        image = self.task_env_config.docker_image
        checkpoint = self._checkpoints.get(self.environment_id)
        if checkpoint is None and image is not None:
            checkpoint = self._checkpoints.get(image)
        if checkpoint is None:
            return None
        self._validate_checkpoint(checkpoint)
        return checkpoint

    def _validate_checkpoint(self, checkpoint: _Checkpoint) -> None:
        requested = {
            "cpus": self._effective_cpus,
            "memory_mb": self._effective_memory_mb,
            "storage_mb": self._effective_storage_mb,
            "gpus": self._effective_gpus,
        }
        requested = {key: value for key, value in requested.items() if value}
        if requested and checkpoint.resources is None:
            raise ValueError(
                "checkpoint.resources must declare capacity when the Harbor task "
                "requests resources"
            )
        for name, needed in requested.items():
            available = checkpoint.resources.get(name) if checkpoint.resources else None
            if available is None or needed > available:
                raise ValueError(
                    f"checkpoint {name}={available!r} cannot satisfy requested "
                    f"{name}={needed!r}"
                )
        if checkpoint.network_mode is None:
            raise ValueError(
                "checkpoint.network_mode must declare the prepared machine's "
                "inherited egress policy"
            )
        requested_mode = self.network_policy.network_mode.value
        if checkpoint.network_mode != requested_mode:
            raise ValueError(
                "checkpoint network_mode does not match the Harbor task policy"
            )
        if requested_mode == NetworkMode.ALLOWLIST.value and set(
            checkpoint.allowed_hosts
        ) != set(self.network_policy.allowed_hosts):
            raise ValueError(
                "checkpoint allowed_hosts do not match the Harbor task policy"
            )

    def _golden_key(self) -> tuple[Any, ...]:
        resources = self._resource_spec()
        return (
            self._target,
            self._connect.base_url,
            self._connect.api_key,
            self.environment_id,
            self._image(),
            resources.cpus,
            resources.memory_mb,
            resources.storage_gb,
            resources.network,
            tuple(resources.allow_hosts or ()),
            tuple(resources.allow_cidrs or ()),
            resources.cuda,
            resources.gpu,
            tuple(sorted(self._startup_env().items())),
            self.task_env_config.workdir,
        )

    async def _create_golden(self) -> AsyncMachine:
        machine = await AsyncMachine.create(
            MachineConfig(
                name=f"harbor-golden-{self.environment_id[:12]}-{uuid.uuid4().hex[:6]}",
                image=self._image(),
                resources=self._resource_spec(),
                checkpoint=True,
                ready_timeout_seconds=self._ready_timeout_sec,
                env=self._startup_env() or None,
                workdir=self.task_env_config.workdir,
            ),
            self._connect,
        )
        _OWNED_GOLDENS.append(machine)
        return machine

    async def _auto_golden(self) -> AsyncMachine:
        state = _loop_state()
        key = self._golden_key()
        task = state.golden_tasks.get(key)
        if task is None:
            task = asyncio.create_task(self._create_golden())
            state.golden_tasks[key] = task
        try:
            return await asyncio.shield(task)
        except BaseException:
            if state.golden_tasks.get(key) is task:
                state.golden_tasks.pop(key, None)
            raise

    async def _external_golden(self, checkpoint: _Checkpoint) -> AsyncMachine:
        return await AsyncMachine.connect(checkpoint.machine, self._connect)

    def _batcher(
        self,
        key: tuple[Any, ...],
        resolve_golden: Callable[[], Awaitable[AsyncMachine]],
    ) -> _ForkBatcher:
        state = _loop_state()
        batcher_key = (
            *key,
            self._fork_batch_window_s,
            self._fork_batch_size,
        )
        batcher = state.batchers.get(batcher_key)
        if batcher is None:
            batcher = _ForkBatcher(
                resolve_golden,
                window_s=self._fork_batch_window_s,
                max_size=self._fork_batch_size,
            )
            state.batchers[batcher_key] = batcher
        return batcher

    @override
    async def start(self, force_build: bool) -> None:
        if self._machine is not None:
            return
        name = _machine_name(f"harbor-{self.session_id}-{uuid.uuid4().hex[:8]}")
        checkpoint = None if force_build else self._resolve_checkpoint()
        try:
            if checkpoint is not None:
                key = (
                    "external",
                    checkpoint.machine,
                    self._target,
                    self._connect.base_url,
                    self._connect.api_key,
                )
                self._machine = await self._batcher(
                    key, lambda: self._external_golden(checkpoint)
                ).submit(name)
            else:
                await self._prepare_image(force_build=force_build)
                if self._auto_checkpoint:
                    key = ("auto", *self._golden_key())
                    self._machine = await self._batcher(key, self._auto_golden).submit(
                        name
                    )
                else:
                    self._machine = await AsyncMachine.create(
                        MachineConfig(
                            name=name,
                            image=self._image(),
                            resources=self._resource_spec(),
                            ready_timeout_seconds=self._ready_timeout_sec,
                            env=self._startup_env() or None,
                            workdir=self.task_env_config.workdir,
                        ),
                        self._connect,
                    )
            await self.ensure_dirs(self._mount_targets(writable_only=True))
            await self._upload_environment_dir_after_start()
        except BaseException:
            machine, self._machine = self._machine, None
            if machine is not None:
                with contextlib.suppress(Exception):
                    await machine.delete()
            raise

    def _require_machine(self) -> AsyncMachine:
        if self._machine is None:
            raise RuntimeError("SmolEnvironment has not been started")
        return self._machine

    @override
    async def stop(self, delete: bool) -> None:
        machine, self._machine = self._machine, None
        if machine is None:
            return
        try:
            if delete:
                await machine.delete()
            else:
                await machine.stop()
        except SmolError as error:
            if error.code not in {"NOT_FOUND", "VM_NOT_FOUND"}:
                self._machine = machine
                raise

    @override
    async def exec(
        self,
        command: str,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        timeout_sec: int | None = None,
        user: str | int | None = None,
    ) -> ExecResult:
        machine = self._require_machine()
        resolved_user = self._resolve_user(user)
        if resolved_user in {None, "root", 0, "0"}:
            argv = ["/bin/sh", "-c", _SHELL_EXEC, "smol-harbor", command]
        else:
            argv = [
                "/bin/sh",
                "-c",
                _SHELL_USER_EXEC,
                "smol-harbor",
                command,
                str(resolved_user),
            ]
        result = await machine.exec(
            argv,
            ExecOptions(
                env=self._merge_env(env),
                workdir=cwd or self.task_env_config.workdir,
                timeout=timeout_sec,
            ),
        )
        callback = self._output_callback()
        if callback is not None:
            if result.stdout:
                await callback(result.stdout, "stdout")
            if result.stderr:
                await callback(result.stderr, "stderr")
        return ExecResult(
            stdout=result.stdout or None,
            stderr=result.stderr or None,
            return_code=result.exit_code,
        )

    @override
    async def upload_file(self, source_path: Path | str, target_path: str) -> None:
        machine = self._require_machine()
        source = Path(source_path)
        parent = str(PurePosixPath(target_path).parent)
        if parent not in {"", "."}:
            result = await self.exec(f"mkdir -p {shlex.quote(parent)}", user="root")
            if result.return_code != 0:
                raise RuntimeError(
                    f"failed to create upload directory {parent!r}: {result.stderr}"
                )
        await machine.write_file(
            target_path,
            source.read_bytes(),
            mode=source.stat().st_mode & 0o777,
        )

    @override
    async def upload_dir(self, source_dir: Path | str, target_dir: str) -> None:
        source = Path(source_dir)
        if not source.is_dir():
            raise NotADirectoryError(source)
        transfer = _TRANSFER_DIR / f"smol-harbor-{uuid.uuid4().hex}.tar.gz"
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "upload.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                for child in source.iterdir():
                    bundle.add(child, arcname=child.name, recursive=True)
            await self._require_machine().write_file(
                str(transfer), archive.read_bytes()
            )
        command = (
            f"mkdir -p {shlex.quote(target_dir)} && "
            f"tar xzf {shlex.quote(str(transfer))} -C {shlex.quote(target_dir)}; "
            f"status=$?; rm -f {shlex.quote(str(transfer))}; exit $status"
        )
        result = await self.exec(command, user="root")
        if result.return_code != 0:
            raise RuntimeError(
                f"failed to upload directory {source} to {target_dir}: {result.stderr}"
            )

    @override
    async def download_file(self, source_path: str, target_path: Path | str) -> None:
        target = Path(target_path)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(await self._require_machine().read_file(source_path))

    @override
    async def download_dir(self, source_dir: str, target_dir: Path | str) -> None:
        transfer = _TRANSFER_DIR / f"smol-harbor-{uuid.uuid4().hex}.tar.gz"
        command = f"tar czf {shlex.quote(str(transfer))} -C {shlex.quote(source_dir)} ."
        result = await self.exec(command, user="root")
        if result.return_code != 0:
            raise RuntimeError(
                f"failed to archive remote directory {source_dir}: {result.stderr}"
            )
        try:
            payload = await self._require_machine().read_file(str(transfer))
        finally:
            with contextlib.suppress(Exception):
                await self.exec(f"rm -f {shlex.quote(str(transfer))}", user="root")
        target = Path(target_dir)
        target.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "download.tar.gz"
            archive.write_bytes(payload)
            with tarfile.open(archive, "r:gz") as bundle:
                bundle.extractall(target, filter="data")


__all__ = ["SmolEnvironment", "close_harbor_goldens"]
