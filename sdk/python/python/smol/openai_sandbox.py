"""Smol microVM provider for the OpenAI Agents SDK's SandboxAgent.

Install ``smolmachines[openai-agents]``. The application owns credentials,
network access and deletion; authentication stays outside serialized session state.
"""

from __future__ import annotations

import io
import shlex
import uuid
from pathlib import Path
from typing import Literal

from agents.sandbox.entries import Mount
from agents.sandbox.errors import WorkspaceReadNotFoundError
from agents.sandbox.files import FileEntry
from agents.sandbox.manifest import Manifest
from agents.sandbox.session import SandboxSession, SandboxSessionState
from agents.sandbox.session.base_sandbox_session import BaseSandboxSession
from agents.sandbox.session.sandbox_client import (
    BaseSandboxClient,
    BaseSandboxClientOptions,
)
from agents.sandbox.snapshot import SnapshotBase, SnapshotSpec, resolve_snapshot
from agents.sandbox.types import ExecResult, User
from agents.sandbox.util.parse_utils import parse_ls_la
from agents.sandbox.util.tar_utils import validate_tar_bytes
from pydantic import Field

from .async_machine import AsyncMachine
from .errors import SmolError
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec


class SmolSandboxClientOptions(BaseSandboxClientOptions):
    type: Literal["smol"] = "smol"
    image: str | None = None
    cpus: int = Field(default=2, ge=1)
    memory_mb: int = Field(default=768, ge=256)
    network: bool = False
    ttl_seconds: int = Field(default=3600, ge=60)


class SmolSandboxSessionState(SandboxSessionState):
    type: Literal["smol"] = "smol"
    machine_id: str
    target: Literal["local", "cloud"]
    options: SmolSandboxClientOptions = Field(default_factory=SmolSandboxClientOptions)


class SmolSandboxSession(BaseSandboxSession):
    state: SmolSandboxSessionState

    def __init__(
        self,
        state: SmolSandboxSessionState,
        conn: ConnectOptions,
        machine: AsyncMachine | None = None,
        allow_network: bool = False,
    ):
        self.state = state
        self._conn = conn
        self._allow_network = allow_network
        self._machine = machine
        self._set_start_state_preserved(machine is None)

    @property
    def machine(self) -> AsyncMachine:
        if self._machine is None:
            raise RuntimeError("Smol sandbox session has not started")
        return self._machine

    async def _ensure_backend_started(self) -> None:
        if self._machine is None:
            try:
                machine = await AsyncMachine.connect(self.state.machine_id, self._conn)
                current = await machine.state()
                if current == "deleted":
                    raise SmolError("NOT_FOUND", "sandbox machine was deleted")
            except SmolError as exc:
                if exc.code != "NOT_FOUND":
                    raise
                if not await self.state.snapshot.restorable(
                    dependencies=self.dependencies
                ):
                    raise RuntimeError(
                        "sandbox machine is gone and its workspace snapshot is unavailable"
                    ) from exc
                machine = await AsyncMachine.create(
                    SmolSandboxClient._machine_config(
                        self.state.options, self.state.target
                    ),
                    self._conn,
                )
                self.state.machine_id = machine.id
                self.state.workspace_root_ready = False
                self._set_start_state_preserved(False)
                current = "started"
            self._machine = machine
            if current == "paused":
                await self.machine.resume()
            elif current in ("stopped", "created"):
                await self.machine.start()
            else:
                await self.machine.wait_until_ready()
        result = await self.machine.exec(["mkdir", "-p", self.state.manifest.root])
        if not result.success:
            raise RuntimeError(f"Smol workspace setup failed: {result.stderr}")

    async def _exec_internal(
        self, *command: str | Path, timeout: float | None = None
    ) -> ExecResult:
        result = await self.machine.exec(
            [str(part) for part in command],
            ExecOptions(timeout=timeout, workdir=self.state.manifest.root),
        )
        return ExecResult(
            stdout=result.stdout_bytes,
            stderr=result.stderr_bytes,
            exit_code=result.exit_code,
        )

    async def read(self, path: Path, *, user: str | User | None = None) -> io.IOBase:
        if user is not None:
            raise ValueError("user-specific file operations are not supported")
        try:
            return io.BytesIO(await self.machine.read_file(str(path)))
        except (FileNotFoundError, SmolError) as exc:
            # Local native errors currently wrap the guest's missing-file errno
            # without preserving its structured code.
            if (
                isinstance(exc, SmolError)
                and exc.code not in ("NOT_FOUND", "FILE_NOT_FOUND")
                and not (exc.code == "SMOLVM_ERROR" and "(os error 2)" in str(exc))
            ):
                raise
            raise WorkspaceReadNotFoundError(path=path) from exc

    async def _read_bounded(self, path: Path, *, max_bytes: int) -> bytes:
        # Exec output is capped by the engine; request a bounded prefix rather
        # than reading a potentially multi-GiB file before applying the limit.
        if max_bytes > 4 * 1024 * 1024:
            raise ValueError("bounded reads above 4 MiB are unsupported")
        quoted = shlex.quote(str(path))
        command = (
            f"test -f {quoted} || exit 42; head -c {max_bytes} -- {quoted} | base64"
        )
        result = await self.machine.exec(["sh", "-c", command], ExecOptions(timeout=30))
        if result.exit_code != 0:
            if result.exit_code == 42:
                raise WorkspaceReadNotFoundError(path=path)
            raise RuntimeError(f"Smol bounded read failed: {result.stderr}")
        import base64

        return base64.b64decode(result.stdout_bytes)

    async def write(
        self, path: Path, data: io.IOBase, *, user: str | User | None = None
    ) -> None:
        if user is not None:
            raise ValueError("user-specific file operations are not supported")
        payload = data.read()
        if isinstance(payload, str):
            payload = payload.encode("utf-8")
        await self.machine.write_file(str(path), payload)

    async def running(self) -> bool:
        return self._machine is not None and await self.machine.ready()

    async def ls(
        self, path: Path | str, *, user: str | User | None = None
    ) -> list[FileEntry]:
        if str(path).rstrip("/") != self.state.manifest.root:
            return await super().ls(path, user=user)
        # Smol exposes /workspace as a symlink to its storage disk. `ls` without
        # the trailing slash would report the symlink rather than its children.
        result = await self.exec(
            "ls", "-la", "--", "/workspace/", shell=False, user=user
        )
        if not result.ok():
            raise RuntimeError(f"Smol workspace listing failed: {result.stderr!r}")
        return parse_ls_la(
            result.stdout.decode("utf-8", errors="replace"), base="/workspace"
        )

    async def _clear_workspace_root_on_resume(self) -> None:
        await self._clear_workspace_contents()

    async def _clear_workspace_contents(self, *, keep: str | None = None) -> None:
        # Keep the /workspace symlink: deleting it disconnects the guest's disk.
        # A snapshot being restored is staged on this same disk and must survive.
        script = (
            f"keep={shlex.quote(keep or '')}; "
            "for entry in /workspace/* /workspace/.[!.]* /workspace/..?*; do "
            'if [ "$entry" = "$keep" ]; then continue; fi; '
            'if [ -e "$entry" ] || [ -L "$entry" ]; then '
            'rm -rf -- "$entry" || exit; fi; done'
        )
        result = await self.machine.exec(["sh", "-c", script])
        if not result.success:
            raise RuntimeError(f"Smol workspace cleanup failed: {result.stderr}")

    async def persist_workspace(self) -> io.IOBase:
        temp = f"/workspace/.smol-agents-export-{uuid.uuid4().hex}.tar"
        root = self.state.manifest.root
        exclusions = [f"--exclude=./{Path(temp).name}"] + [
            f"--exclude=./{path}" for path in self._persist_workspace_skip_relpaths()
        ]
        try:
            result = await self.machine.exec(
                ["tar", "-C", root, "-cf", temp, *exclusions, "."]
            )
            if not result.success:
                raise RuntimeError(f"Smol workspace export failed: {result.stderr}")
            archive = await self.machine.read_file(temp)
            validate_tar_bytes(archive, allow_external_symlink_targets=False)
            return io.BytesIO(archive)
        finally:
            await self.machine.exec(["rm", "-f", temp])

    async def hydrate_workspace(self, data: io.IOBase) -> None:
        archive = data.read()
        if not isinstance(archive, bytes):
            raise TypeError("workspace archive must contain bytes")
        validate_tar_bytes(archive, allow_external_symlink_targets=False)
        temp = f"/workspace/.smol-agents-import-{uuid.uuid4().hex}.tar"
        try:
            await self.machine.write_file(temp, archive)
            # The archive has been validated, but old workspace symlinks could
            # still redirect extraction outside the root. Clear all entries first.
            await self._clear_workspace_contents(keep=temp)
            result = await self.machine.exec(
                ["tar", "-C", self.state.manifest.root, "-xf", temp]
            )
            if not result.success:
                raise RuntimeError(f"Smol workspace import failed: {result.stderr}")
        finally:
            await self.machine.exec(["rm", "-f", temp])


class SmolSandboxClient(BaseSandboxClient[SmolSandboxClientOptions]):
    """Isolated local/Cloud sandbox sessions with persistent VM identity.

    Call ``delete(session)`` when no further resume is needed. ``aclose()``
    persists the Agents snapshot but retains the VM for reattachment.
    """

    backend_id = "smol"
    supports_default_options = True

    def __init__(
        self,
        *,
        target: Literal["local", "cloud"] = "local",
        conn: ConnectOptions | None = None,
        allow_network: bool = False,
    ):
        if target not in ("local", "cloud"):
            raise ValueError("target must be local or cloud")
        self.target = target
        self.allow_network = allow_network
        self.conn = conn if conn is not None else ConnectOptions(target=target)
        if self.conn.target != target:
            raise ValueError("connection target does not match sandbox target")

    @staticmethod
    def _validate_manifest(manifest: Manifest) -> None:
        if manifest.root != "/workspace":
            raise ValueError("Smol sandbox currently requires /workspace as its root")
        if any(isinstance(entry, Mount) for entry in manifest.entries.values()):
            raise ValueError("Smol sandbox does not support manifest mounts")
        if manifest.extra_path_grants:
            raise ValueError("Smol sandbox does not support extra path grants")
        if manifest.users or manifest.groups:
            raise ValueError("Smol sandbox does not support manifest users or groups")

    @staticmethod
    def _machine_config(opts: SmolSandboxClientOptions, target: str) -> MachineConfig:
        return MachineConfig(
            image=opts.image,
            resources=ResourceSpec(
                cpus=opts.cpus, memory_mb=opts.memory_mb, network=opts.network
            ),
            persistent=True,
            ttl_seconds=opts.ttl_seconds if target == "cloud" else None,
        )

    async def create(
        self,
        *,
        snapshot: SnapshotSpec | SnapshotBase | None = None,
        manifest: Manifest | None = None,
        options: SmolSandboxClientOptions | None = None,
    ) -> SandboxSession:
        manifest = self._validate_manifest_for_create(manifest or Manifest())
        self._validate_manifest(manifest)
        opts = options or SmolSandboxClientOptions()
        if opts.network and not self.allow_network:
            raise ValueError("network access requires allow_network=True on the client")
        if self.target == "cloud" and not opts.image:
            raise ValueError("Cloud sandboxes require an image, such as alpine:3.20")
        machine = await AsyncMachine.create(
            self._machine_config(opts, self.target), self.conn
        )
        try:
            state = SmolSandboxSessionState(
                machine_id=machine.id,
                target=self.target,
                options=opts,
                manifest=manifest,
                snapshot=resolve_snapshot(snapshot, machine.id),
            )
            return self._wrap_session(
                SmolSandboxSession(state, self.conn, machine, self.allow_network)
            )
        except BaseException:
            await machine.delete()
            raise

    async def resume(self, state: SandboxSessionState) -> SandboxSession:
        if (
            not isinstance(state, SmolSandboxSessionState)
            or state.target != self.target
        ):
            raise ValueError("Smol sandbox state and client target must match")
        state.assert_path_grants_rebound()
        self._validate_manifest(state.manifest)
        return self._wrap_session(
            SmolSandboxSession(state, self.conn, allow_network=self.allow_network)
        )

    async def delete(self, session: SandboxSession) -> SandboxSession:
        inner = session._inner
        if not isinstance(inner, SmolSandboxSession):
            raise TypeError("expected a Smol sandbox session")
        machine = inner._machine or await AsyncMachine.connect(
            inner.state.machine_id, self.conn
        )
        await machine.delete()
        inner._machine = None
        return session

    def deserialize_session_state(
        self, payload: dict[str, object]
    ) -> SandboxSessionState:
        return self._deserialize_session_state_payload(payload, SmolSandboxSessionState)
