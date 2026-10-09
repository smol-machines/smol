"""Pydantic AI workspace backed by a local, network-isolated Smol microVM."""

from __future__ import annotations

import asyncio
import math
import uuid
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from .async_machine import AsyncMachine
from .errors import SmolError
from .types import ExecOptions, MachineConfig, ResourceSpec

try:
    from pydantic_ai.capabilities import AbstractCapability
    from pydantic_ai.exceptions import UserError
    from pydantic_ai.tools import AgentDepsT, RunContext
    from pydantic_ai.workspaces import (
        CommandResult,
        WorkspaceBackend,
        WorkspaceCommand,
        WorkspaceRef,
        WorkspaceOutputLimitError,
        WorkspaceTimeoutError,
        WorkspaceUnavailableError,
    )
except ImportError as exc:
    raise ImportError('Install smolmachines[pydantic-ai] to use SmolWorkspace') from exc

_PROVIDER = 'smol-local'
_WORKSPACE = '/workspace'
_MAX_OUTPUT_BYTES = 8 * 1024 * 1024


class SmolWorkspaceBackend:
    """Lazy command backend; Pydantic AI supplies portable file operations via shell."""

    def __init__(
        self,
        ref: WorkspaceRef | None = None,
        *,
        image: str | None = None,
        allow_hosts: Sequence[str] | None = None,
    ) -> None:
        if ref is not None and ref.provider != _PROVIDER:
            raise ValueError(f'expected a {_PROVIDER!r} workspace reference')
        if allow_hosts is not None and not allow_hosts:
            raise ValueError('allow_hosts must contain at least one hostname')
        self._image = image
        self._allow_hosts = tuple(allow_hosts) if allow_hosts is not None else None
        self._ref = ref
        self._machine: AsyncMachine | None = None
        self._acquisition: asyncio.Task[AsyncMachine] | None = None

    @property
    def ref(self) -> WorkspaceRef | None:
        return self._ref

    async def _acquire(self) -> AsyncMachine:
        if self._acquisition is not None:
            return await asyncio.shield(self._acquisition)
        if self._machine is not None:
            return self._machine
        self._acquisition = asyncio.create_task(self._create_or_connect())
        # A cancelled caller must not leave a background task's failure unobserved.
        self._acquisition.add_done_callback(lambda task: task.cancelled() or task.exception())
        return await asyncio.shield(self._acquisition)

    async def _create_or_connect(self) -> AsyncMachine:
        if self._ref is None:
            name = f'pydantic-ai-{uuid.uuid4().hex}'
            resources = ResourceSpec(allow_hosts=list(self._allow_hosts)) if self._allow_hosts is not None else None
            machine = await AsyncMachine.create(
                MachineConfig(name=name, image=self._image, resources=resources, persistent=True)
            )
            self._ref = WorkspaceRef(provider=_PROVIDER, id=machine.id)
            self._machine = machine
            setup = await machine.exec(['mkdir', '-p', _WORKSPACE])
            if setup.exit_code:
                raise RuntimeError(f'could not prepare workspace: {setup.stderr}')
        else:
            try:
                machine = await AsyncMachine.connect(self._ref.id)
                await machine.wait_until_ready()
            except SmolError as exc:
                if exc.code == 'NOT_FOUND':
                    raise WorkspaceUnavailableError(f'Smol workspace {self._ref.id!r} no longer exists') from exc
                raise
            self._machine = machine
            check = await machine.exec(['test', '-d', _WORKSPACE])
            if check.exit_code:
                raise WorkspaceUnavailableError(f'Smol workspace {self._ref.id!r} has no {_WORKSPACE}')
        return machine

    async def working_dir(self) -> str:
        await self._acquire()
        return _WORKSPACE

    async def run(
        self,
        command: WorkspaceCommand,
        *,
        shell: bool = False,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
    ) -> CommandResult:
        if timeout is not None and (not math.isfinite(timeout) or timeout <= 0):
            raise ValueError('timeout must be positive and finite')
        if shell:
            if not isinstance(command, str):
                raise TypeError('shell=True requires a command string')
            argv = ['sh', '-c', command]
        else:
            if isinstance(command, str):
                raise TypeError('shell=False requires an argv sequence')
            argv = list(command)
            if not argv or not all(isinstance(part, str) for part in argv):
                raise ValueError('command must be a nonempty argv of strings')
        machine = await self._acquire()
        stdout: list[str] = []
        stderr: list[str] = []
        captured_bytes = 0
        exit_code: int | None = None
        options = ExecOptions(workdir=_WORKSPACE, env=dict(env) if env is not None else None)
        stream = machine.exec_stream(argv, options)
        try:
            async with asyncio.timeout(timeout):
                async for event in stream:
                    kind = event.get('kind')
                    if kind in ('stdout', 'stderr'):
                        chunk = event.get('data', '')
                        encoded = chunk.encode('utf-8')
                        remaining = _MAX_OUTPUT_BYTES - captured_bytes
                        if len(encoded) > remaining:
                            prefix = encoded[:remaining].decode('utf-8', 'ignore')
                            (stdout if kind == 'stdout' else stderr).append(prefix)
                            raise WorkspaceOutputLimitError(
                                'Smol command exceeded the 8 MiB output limit',
                                limit=_MAX_OUTPUT_BYTES,
                                stdout=''.join(stdout),
                                stderr=''.join(stderr),
                            )
                        captured_bytes += len(encoded)
                        (stdout if kind == 'stdout' else stderr).append(chunk)
                    elif kind == 'exit':
                        exit_code = event['exit_code']
                    elif kind == 'error':
                        raise RuntimeError(event.get('message', 'Smol command stream failed'))
            if exit_code is None:
                raise WorkspaceUnavailableError('Smol command stream ended before the process exited')
        except TimeoutError as exc:
            raise WorkspaceTimeoutError(
                f'command exceeded {timeout}s', stdout=''.join(stdout), stderr=''.join(stderr)
            ) from exc
        except SmolError as exc:
            if exc.code == 'NOT_FOUND':
                raise WorkspaceUnavailableError(
                    f'Smol workspace {self._ref.id if self._ref is not None else "unknown"!r} no longer exists'
                ) from exc
            raise
        finally:
            # Closing an unfinished stream kills the guest command on the local target.
            await stream.aclose()
        # A persistent local machine can outlive this process. Flush the guest's
        # filesystem before reporting a completed command or file operation:
        # otherwise a process exit can lose a just-written workspace file.
        flushed = await machine.exec(['sync'], ExecOptions(timeout=30))
        if flushed.exit_code:
            raise WorkspaceUnavailableError('Smol could not flush the workspace to disk')
        return CommandResult(exit_code=exit_code, stdout=''.join(stdout), stderr=''.join(stderr))



@dataclass(kw_only=True)
class SmolWorkspace(AbstractCapability[AgentDepsT]):
    """Give Pydantic AI's `Shell` and `FileSystem` a persistent local microVM.

    The application owns deletion; completed agent runs leave the VM available for
    resumption via `result.workspace.ref`. Local VMs start with no outbound network.
    """

    image: str | None = None
    """Optional local OCI image archive or cached image. The default uses Smol's BusyBox rootfs."""

    allow_hosts: Sequence[str] | None = None
    """Guest egress hostname allowlist, including image registry hosts when pulling an image."""

    def __post_init__(self) -> None:
        if self.defer_loading:
            raise UserError('SmolWorkspace cannot use defer_loading: the workspace is chosen when the run starts')
        if self.allow_hosts is not None and not self.allow_hosts:
            raise ValueError('allow_hosts must contain at least one hostname')

    def backend(self, ref: WorkspaceRef) -> SmolWorkspaceBackend:
        """Build a backend for an existing Smol workspace, without I/O."""
        return SmolWorkspaceBackend(ref, image=self.image, allow_hosts=self.allow_hosts)

    async def destroy(self, ref: WorkspaceRef) -> None:
        """Delete the local VM named by a workspace reference."""
        if ref.provider != _PROVIDER:
            raise ValueError(f'expected a {_PROVIDER!r} workspace reference')
        machine = await AsyncMachine.connect(ref.id)
        await machine.delete()

    def get_workspace(self, ctx: RunContext[AgentDepsT], *, ref: WorkspaceRef | None) -> WorkspaceBackend | None:
        """Provide a lazy Smol backend for new and matching resumed runs."""
        del ctx
        if ref is not None and ref.provider != _PROVIDER:
            return None
        return SmolWorkspaceBackend(ref, image=self.image, allow_hosts=self.allow_hosts)
