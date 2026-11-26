"""Ray Sandboxes backend backed by local SmolVM or Smol Cloud.

Install ``smolmachines[ray]`` and call :func:`use_smol` before ``ray.init()``.
``ray.experimental.sandbox.create`` / ``restore`` then run each sandbox as a
smol machine: its own Linux kernel under KVM (or Hypervisor.framework), locally
through the embedded engine or in Smol Cloud on nodes without ``/dev/kvm``.

    import ray
    from ray.experimental import sandbox
    from smol import ray_sandbox

    ray.init(runtime_env=ray_sandbox.use_smol())        # or use_smol(target="cloud")
    sb = sandbox.create(image="python:3.12-slim", cpu=0, workdir="/workspace")
    ray.get(sb.exec.remote("python3 -V"))
    path = ray.get(sb.checkpoint.remote("/tmp/ckpt/base"))
    restored = sandbox.restore(path)
    children = ray_sandbox.branch(sb, 20)               # live copy-on-write branches

Works with Ray's sandbox package as released (Ray 2.58+: create, exec, files,
status) and as extended with checkpoint, restore, pause and resume
(ray-project/ray#66471), plus branching of a running sandbox, which needs no
checkpoint round trip.
"""

from __future__ import annotations

import json
import math
import os
import shutil
import time
import uuid
from typing import Any, Dict, List, Optional, Union

from ray.experimental.sandbox.backend.base import (
    BaseSandboxBackend,
    ExecResult,
    SandboxStatus,
)
from ray.experimental.sandbox.config import SandboxConfig, parse_memory_bytes
from ray.experimental.sandbox.exceptions import (
    SandboxCreationError,
    SandboxError,
    SandboxExecError,
    SandboxNotFoundError,
    SandboxTimeoutError,
)
from ray.experimental.sandbox.image_manager import BaseImageManager

from .errors import SmolError
from .machine import Machine
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

# Selects local SmolVM or Smol Cloud in every Ray process (driver and workers).
TARGET_ENV = "RAY_SANDBOX_SMOL_TARGET"

_ARTIFACT = "sandbox.smolcheckpoint"
_DEFAULT_MEMORY_MB = 1024
_TIMEOUT_EXIT = 124

# SandboxStatus gained PAUSED with checkpoint/restore; released Ray has no
# pause, so a paused machine reads as running there.
_PAUSED = getattr(SandboxStatus, "PAUSED", SandboxStatus.RUNNING)
_STATUS = {
    "running": SandboxStatus.RUNNING,
    "started": SandboxStatus.RUNNING,
    "paused": _PAUSED,
    "created": SandboxStatus.PENDING,
    "creating": SandboxStatus.PENDING,
    "starting": SandboxStatus.PENDING,
    "stopped": SandboxStatus.TERMINATED,
    "deleted": SandboxStatus.TERMINATED,
    "error": SandboxStatus.ERROR,
}


class _NoImageManager(BaseImageManager):
    """smol pulls and caches images itself; Ray's EROFS image cache is unused."""

    def pull_image(self, image, timeout_seconds=120.0, instance_id=None) -> str:
        return ""

    def get_image_dir(self, image):
        return ""

    def get_image_config(self, image):
        return {}

    def get_workdir(self, image):
        return None

    def get_envs(self, image):
        return []

    def create_oci_spec(self, *args, **kwargs):
        raise NotImplementedError("smol machines run images natively, not OCI bundles")

    def prepare_oci_bundle(self, *args, **kwargs):
        raise NotImplementedError("smol machines run images natively, not OCI bundles")

    def get_rootfs_path(self, *args, **kwargs):
        raise NotImplementedError("smol machines run images natively, not OCI bundles")


# Ray's image-manager interface moves between releases (2.59 has
# get_rootfs_path, #66471 does not). Nothing here is ever called on it, so do
# not let a new abstract method make the backend unconstructible.
_NoImageManager.__abstractmethods__ = frozenset()


def _manifest(sandbox_id: str, config: SandboxConfig, cwd: str, leave_running: bool) -> dict:
    """A checkpoint manifest in the layout Ray's ``sandbox.restore()`` reads."""
    return {
        "version": "1.0",
        "created_at": time.time(),
        "sandbox_id": sandbox_id,
        "image": config.image,
        "cwd": cwd,
        "workdir": config.workdir,
        "config": _config_fields(config),
        "state_dir": "state",
        "fs_dir": "fs",
        "leave_running": leave_running,
    }


def _config_fields(config: SandboxConfig) -> dict:
    """The config's public fields that survive JSON, whatever this Ray version
    defines (fields come and go between releases: ``readonly`` was removed)."""
    out = {}
    for name in getattr(config, "__dataclass_fields__", {}):
        if name.startswith("_"):
            continue
        value = getattr(config, name, None)
        try:
            json.dumps(value)
        except (TypeError, ValueError):
            continue
        out[name] = value
    return out


def _load_manifest(checkpoint_path: str) -> tuple[dict, str]:
    manifest_file = os.path.join(checkpoint_path, "manifest.json")
    if not os.path.isfile(manifest_file):
        raise SandboxError(f"no manifest.json in checkpoint directory '{checkpoint_path}'")
    with open(manifest_file, encoding="utf-8") as f:
        return json.load(f), os.path.join(checkpoint_path, "state")


def _text(raw: bytes, text: str) -> str:
    return raw.decode(errors="replace") if raw else text


def _connect_options(target: Optional[str] = None) -> ConnectOptions:
    target = target or os.environ.get(TARGET_ENV) or "local"
    if target not in ("local", "cloud"):
        raise ValueError(f"{TARGET_ENV} must be 'local' or 'cloud', not {target!r}")
    return ConnectOptions(target=target)


class SmolSandboxBackend(BaseSandboxBackend):
    """Ray sandbox backend that runs each sandbox as a smol machine."""

    def __init__(
        self,
        image_manager: Optional[BaseImageManager] = None,
        conn: Optional[ConnectOptions] = None,
        default_memory_mb: int = _DEFAULT_MEMORY_MB,
    ):
        super().__init__(image_manager=image_manager or _NoImageManager())
        self._conn = conn or _connect_options()
        self._default_memory_mb = default_memory_mb
        self._machines: Dict[str, Machine] = {}
        self._sandbox_metadata: Dict[str, Dict[str, Any]] = {}

    @property
    def target(self) -> str:
        return self._conn.target or "local"

    # ---- lifecycle ----------------------------------------------------------

    def _machine_config(self, config: SandboxConfig, name: str) -> MachineConfig:
        mem = parse_memory_bytes(config.memory) or 0
        return MachineConfig(
            name=name,
            image=config.image,
            resources=ResourceSpec(
                cpus=max(1, math.ceil(config.cpu or 0)),
                memory_mb=max(256, mem // (1024 * 1024)) if mem else self._default_memory_mb,
            ),
            # "none" stays offline; Ray's other modes all mean egress.
            network=config.network != "none",
            env=dict(config.env) if config.env else None,
            workdir=config.workdir,
            # Branchable so branch() needs no checkpoint; persistent so another
            # Ray process (the actor that owns a branch) can attach by name.
            branchable=True,
            persistent=True,
            ttl_seconds=config.ttl_seconds if (config.ttl_seconds or 0) > 0 else None,
            ready_timeout_seconds=max(float(config.timeout_seconds), 120.0),
        )

    def _register(self, sandbox_id: str, machine: Machine, config: SandboxConfig, **extra) -> None:
        self._machines[sandbox_id] = machine
        self._sandbox_metadata[sandbox_id] = {
            "config": config,
            "cwd": config.workdir or "/",
            "workdir": config.workdir,
            "status": SandboxStatus.RUNNING,
            **extra,
        }

    def _machine(self, sandbox_id: str) -> Machine:
        machine = self._machines.get(sandbox_id)
        if machine is None:
            raise SandboxNotFoundError(f"sandbox '{sandbox_id}' is not owned by this backend")
        return machine

    def create_sandbox(self, config: SandboxConfig) -> str:
        name = f"ray-sb-{uuid.uuid4().hex[:12]}"
        try:
            machine = Machine.create(self._machine_config(config, name), self._conn)
        except SmolError as err:
            raise SandboxCreationError(f"smol machine create failed: {err}") from err
        sandbox_id = machine.id if self.target == "cloud" else name
        self._register(sandbox_id, machine, config)
        if config.workdir:
            # gVisor bind-mounts an explicit workdir; on a VM it is a directory.
            machine.exec(["mkdir", "-p", config.workdir])
        return sandbox_id

    def delete_sandbox(self, sandbox_id: str) -> None:
        machine = self._machines.pop(sandbox_id, None)
        if machine is not None:
            try:
                machine.delete()
            except SmolError:
                pass
        meta = self._sandbox_metadata.get(sandbox_id)
        if meta:
            meta["status"] = SandboxStatus.TERMINATED

    def get_status(self, sandbox_id: str) -> SandboxStatus:
        machine = self._machines.get(sandbox_id)
        if machine is None:
            return SandboxStatus.TERMINATED
        try:
            return _STATUS.get(machine.state(), SandboxStatus.ERROR)
        except SmolError:
            return SandboxStatus.TERMINATED

    def get_termination_reason(self, sandbox_id: str) -> Optional[str]:
        """Why a sandbox this backend still owns stopped on its own, or None if
        it is running, was deleted, or is unknown."""
        if sandbox_id not in self._machines:
            return None
        if self.get_status(sandbox_id) == SandboxStatus.TERMINATED:
            return "smol machine stopped unexpectedly"
        return None

    def get_config(self, sandbox_id: str) -> SandboxConfig:
        meta = self._sandbox_metadata.get(sandbox_id)
        if meta is None:
            raise SandboxNotFoundError(f"sandbox '{sandbox_id}' is not owned by this backend")
        return meta["config"]

    def sandbox_dir(self, sandbox_id: str) -> str:
        """A host directory for this sandbox's scratch files."""
        self.get_config(sandbox_id)
        path = os.path.join("/tmp/ray/sandbox/smol", sandbox_id)
        os.makedirs(path, exist_ok=True)
        return path

    def mount_points(self, sandbox_id: str) -> List[str]:
        """Nothing is bind-mounted into a smol machine by this backend."""
        self.get_config(sandbox_id)
        return []

    def export_rootfs_changes(self, sandbox_id: str, path: str, timeout: float = 1800.0) -> None:
        raise SandboxError(_COMMIT_UNSUPPORTED)

    # ---- exec and files -----------------------------------------------------

    def exec_command(
        self,
        sandbox_id: str,
        command: Union[str, List[str]],
        timeout: Optional[float] = None,
        cwd: Optional[str] = None,
        env: Optional[Dict[str, str]] = None,
        shell: Optional[str] = None,
        user: Optional[str] = None,
        combine_output: bool = False,
        **kwargs,
    ) -> ExecResult:
        machine = self._machine(sandbox_id)
        meta = self._sandbox_metadata[sandbox_id]
        if isinstance(command, list):
            argv = command
        else:
            argv = [shell or meta["config"].shell, "-c", command]
        if combine_output:
            # One stream, in the order the command wrote it, as gVisor returns it.
            argv = ["/bin/sh", "-c", 'exec "$@" 2>&1', "sh", *argv]
        options = ExecOptions(
            env=env or None,
            workdir=cwd or meta.get("workdir"),
            timeout=timeout,
            user=user,
            output="both",
        )
        start = time.time()
        try:
            result = machine.exec(argv, options)
        except SmolError as err:
            raise SandboxExecError(f"smol exec failed: {err}") from err
        duration = time.time() - start
        if timeout and result.exit_code == _TIMEOUT_EXIT and duration >= timeout:
            raise SandboxTimeoutError(f"smol exec timed out after {timeout}s")
        if combine_output:
            # Ray releases with `combine_output` also have ExecResult.output.
            return ExecResult(
                exit_code=result.exit_code,
                stdout="",
                stderr="",
                duration_seconds=duration,
                output=result.stdout_bytes or result.stdout.encode(),
            )
        return ExecResult(
            exit_code=result.exit_code,
            stdout=_text(result.stdout_bytes, result.stdout),
            stderr=_text(result.stderr_bytes, result.stderr),
            duration_seconds=duration,
        )

    def _path(self, sandbox_id: str, path: str) -> str:
        if path.startswith("/"):
            return path
        return os.path.join(self._sandbox_metadata[sandbox_id]["cwd"], path)

    def write_file(self, sandbox_id, path, content, append=False) -> None:
        machine = self._machine(sandbox_id)
        path = self._path(sandbox_id, path)
        data = content.encode() if isinstance(content, str) else bytes(content)
        if append:
            try:
                data = machine.read_file(path) + data
            except SmolError:
                pass
        machine.write_file(path, data)

    def read_file(self, sandbox_id, path) -> bytes:
        try:
            return self._machine(sandbox_id).read_file(self._path(sandbox_id, path))
        except SmolError as err:
            raise SandboxError(f"smol read_file failed: {err}") from err

    # ---- pause / resume -----------------------------------------------------

    def pause_sandbox(self, sandbox_id, timeout_seconds=None) -> None:
        # A smol pause saves RAM and disk and stops the VM, releasing its memory;
        # closer to gVisor's checkpoint(leave_running=False) than to runsc pause.
        self._machine(sandbox_id).pause()
        self._sandbox_metadata[sandbox_id]["status"] = _PAUSED

    def resume_sandbox(self, sandbox_id, timeout_seconds=None) -> None:
        self._machine(sandbox_id).resume()
        self._sandbox_metadata[sandbox_id]["status"] = SandboxStatus.RUNNING

    # ---- checkpoint / restore -----------------------------------------------

    def checkpoint_sandbox(
        self,
        sandbox_id: str,
        checkpoint_path: Optional[str] = None,
        leave_running: bool = True,
        compression: str = "none",
        exclude_committed_zero_pages: bool = True,
        direct: bool = False,
        timeout_seconds: float = 300.0,
        **kwargs,
    ) -> Dict[str, Any]:
        """Write a Ray-format bundle that ``sandbox.restore()`` accepts.

        Locally the bundle holds the portable artifact (RAM, vCPU state and
        disks) in ``state/``. In Smol Cloud the checkpoint stays durable in the
        cloud and the bundle records its id, so nothing large crosses the wire.
        """
        machine = self._machine(sandbox_id)
        meta = self._sandbox_metadata[sandbox_id]
        config: SandboxConfig = meta["config"]
        checkpoint_path = os.path.abspath(
            checkpoint_path
            or os.path.join("/tmp/ray/sandbox/checkpoints", f"{sandbox_id}-{uuid.uuid4().hex[:8]}")
        )
        start = time.time()
        staging = checkpoint_path + ".staging"
        shutil.rmtree(staging, ignore_errors=True)
        os.makedirs(os.path.join(staging, "state"))
        try:
            if self.target == "cloud":
                info = machine.checkpoint()
            else:
                info = machine.checkpoint(os.path.join(staging, "state", _ARTIFACT))
            manifest = _manifest(sandbox_id, config, meta["cwd"], leave_running)
            manifest["backend"] = "smol"
            manifest["smol_target"] = self.target
            manifest["smol_checkpoint_id"] = info.id
            with open(os.path.join(staging, "manifest.json"), "w", encoding="utf-8") as f:
                json.dump(manifest, f, indent=2)
            shutil.rmtree(checkpoint_path, ignore_errors=True)
            os.rename(staging, checkpoint_path)
        except SmolError as err:
            shutil.rmtree(staging, ignore_errors=True)
            raise SandboxError(f"smol checkpoint failed: {err}") from err
        except BaseException:
            shutil.rmtree(staging, ignore_errors=True)
            raise
        if not leave_running:
            self.delete_sandbox(sandbox_id)
        return {
            "checkpoint_path": checkpoint_path,
            "sandbox_id": sandbox_id,
            "duration_seconds": time.time() - start,
            "leave_running": leave_running,
            "image": config.image,
            "smol_checkpoint_id": info.id,
        }

    def restore_sandbox(
        self,
        checkpoint_path: str,
        cpu: Optional[float] = None,
        memory: Optional[Union[str, int, float]] = None,
        background: bool = True,
        direct: bool = False,
        timeout_seconds: float = 300.0,
        **kwargs,
    ) -> str:
        manifest, state_dir = _load_manifest(checkpoint_path)
        if manifest.get("backend") != "smol":
            raise SandboxError(f"'{checkpoint_path}' is not a smol checkpoint")
        known = SandboxConfig.__dataclass_fields__
        config = SandboxConfig(
            **{k: v for k, v in manifest["config"].items() if k in known and not k.startswith("_")}
        )
        # A cloud checkpoint restores by id; a local artifact restores from the
        # file, on this node or (uploaded) in the cloud.
        source = (
            manifest["smol_checkpoint_id"]
            if manifest.get("smol_target") == "cloud" and self.target == "cloud"
            else os.path.join(state_dir, _ARTIFACT)
        )
        name = f"ray-sb-{uuid.uuid4().hex[:12]}"
        try:
            machine = Machine.restore_checkpoint(
                source, name, self._conn, network=config.network != "none"
            )
        except SmolError as err:
            raise SandboxCreationError(f"smol restore failed: {err}") from err
        sandbox_id = machine.id if self.target == "cloud" else name
        self._register(sandbox_id, machine, config, restored_from=checkpoint_path)
        return sandbox_id

    def attach_sandbox(self, sandbox_id: str, workdir: Optional[str] = None) -> str:
        """Own a machine created elsewhere, such as a branch made by the driver."""
        try:
            machine = Machine.connect(sandbox_id, self._conn)
        except SmolError as err:
            raise SandboxNotFoundError(f"cannot attach smol machine '{sandbox_id}': {err}") from err
        self._register(sandbox_id, machine, SandboxConfig(image="smol-branch", workdir=workdir))
        return sandbox_id

    # ---- smol extra: live branching -----------------------------------------

    def branch_sandboxes(self, parent_id: str, count: int) -> List[str]:
        """Branch a running sandbox into ``count`` copy-on-write children.

        Children start from the parent's live RAM and disk at this moment, in
        one batch, with no checkpoint written.
        """
        parent = self._machine(parent_id) if parent_id in self._machines else Machine.connect(
            parent_id, self._conn
        )
        names = [f"ray-sb-{uuid.uuid4().hex[:12]}" for _ in range(count)]
        try:
            children = parent.branch_batch(names=names)
        except SmolError as err:
            raise SandboxCreationError(f"smol branch failed: {err}") from err
        config = self._sandbox_metadata.get(parent_id, {}).get("config") or SandboxConfig(
            image="smol-branch"
        )
        ids = []
        for name, child in zip(names, children):
            child_id = child.id if self.target == "cloud" else name
            self._register(child_id, child, config, parent=parent_id)
            ids.append(child_id)
        return ids


# ---- wiring into Ray ---------------------------------------------------------


_COMMIT_UNSUPPORTED = (
    "commit() is not supported by the smol backend: a smol machine is a VM, not "
    "an overlay on a cached image. Use checkpoint()/restore() or ray_sandbox.branch()."
)


def _runtime_class():
    from ray.experimental.sandbox.runtime import SandboxRuntime

    class SmolSandboxRuntime(SandboxRuntime):
        """SandboxRuntime with the smol backend in place of gVisor."""

        def __init__(self):
            self._image_manager = _NoImageManager()
            self._backend = SmolSandboxBackend(image_manager=self._image_manager)
            self._ttl_timers = {}

        def commit(self, instance_id, image, env=None, timeout_seconds=1800.0, **kwargs):
            # Refuse before Ray's commit pauses the sandbox or reads an image cache.
            self._backend.get_config(instance_id)
            raise SandboxError(_COMMIT_UNSUPPORTED)

    return SmolSandboxRuntime


def _install_runtime() -> None:
    import ray.experimental.sandbox as pkg
    import ray.experimental.sandbox.runtime as runtime_mod
    import ray.experimental.sandbox.sandbox as sandbox_mod

    if getattr(sandbox_mod.SandboxRuntime, "_smol", False):
        return
    cls = _runtime_class()
    cls._smol = True
    sandbox_mod.SandboxRuntime = cls
    runtime_mod.SandboxRuntime = cls
    pkg.SandboxRuntime = cls


_ACTOR = None


def sandbox_actor():
    """Ray's Sandbox actor class, constructing its runtime with smol.

    A subclass rather than a runtime_env setup hook: a custom runtime_env
    keeps Ray from reusing prestarted workers, which costs every sandbox actor
    a cold Python start.
    """
    global _ACTOR
    if _ACTOR is None:
        import ray
        import ray.experimental.sandbox.sandbox as sandbox_mod

        base = sandbox_mod.Sandbox.__ray_actor_class__

        class Sandbox(base):
            def __init__(self, *args, smol_attach=None, smol_workdir=None, **kwargs):
                _install_runtime()
                if smol_attach is None:
                    super().__init__(*args, **kwargs)
                    return
                # Adopt a machine that already runs (a branch made by branch()):
                # same actor, same methods, nothing created.
                self.runtime = sandbox_mod.SandboxRuntime()
                self.instance_id = self.runtime.backend.attach_sandbox(smol_attach, smol_workdir)

        _ACTOR = ray.remote(Sandbox)
    return _ACTOR


def use_smol(target: Optional[str] = None) -> Dict[str, Any]:
    """Route ``ray.experimental.sandbox`` to smol machines; call before ``ray.init()``.

    ``target`` is ``"local"`` (default) or ``"cloud"`` (``SMOL_CLOUD_TOKEN``).
    It is exported to the Ray workers through the environment they inherit.
    Workers need ``smolmachines`` installed, as for any Ray dependency.
    Returns the runtime_env to pass to ``ray.init`` (empty: none is needed).
    """
    import ray.experimental.sandbox as pkg

    if target:
        _connect_options(target)
        os.environ[TARGET_ENV] = target
    _install_runtime()
    pkg.Sandbox = sandbox_actor()
    return {}


def branch(parent, n: int = 1, **actor_options) -> list:
    """Branch a running Sandbox actor into ``n`` new Sandbox actors.

    The children are branched together in one batch from the parent's live
    state, then each is handed to its own ordinary Sandbox actor (exec, files,
    checkpoint, delete all work).
    """
    import ray

    parent_id = ray.get(parent.get_instance_id.remote())
    workdir = ray.get(parent.get_config.remote()).workdir
    children = SmolSandboxBackend().branch_sandboxes(parent_id, n)
    actor_options.setdefault("num_cpus", 0)
    return [
        sandbox_actor()
        .options(**actor_options)
        .remote(smol_attach=child, smol_workdir=workdir)
        for child in children
    ]
