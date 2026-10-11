"""Run Strands agent shell and file tools inside a local or cloud Smol VM.

Install ``smolmachines[strands]`` and pass ``SmolSandbox`` to ``Agent``.
The sandbox owns machines it creates; an injected machine stays caller-owned.
"""

from __future__ import annotations

import asyncio
import math
import threading
import time
from collections.abc import AsyncGenerator
from typing import Any

from strands.sandbox import ExecutionResult, PosixShellSandbox, StreamChunk
from strands.sandbox.errors import SandboxTimeoutError
from strands.sandbox.posix_shell import validate_env_keys
from strands.types.tools import AgentTool
from strands.vended_tools.file_editor import make_file_editor
from strands.vended_tools.file_editor.file_editor import DEFAULT_FILE_EDITOR_DESCRIPTION
from strands.vended_tools.shell import make_shell
from strands.vended_tools.shell.types import SANDBOX_SHELL_DESCRIPTION
from typing_extensions import Self  # noqa: UP035 - smolmachines supports Python 3.9.

from .errors import SmolError
from .machine import Machine
from .transport import ExecStream
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec


def _next_event(stream: ExecStream) -> dict[str, Any] | None:
    """Keep StopIteration inside the worker thread (asyncio cannot carry it)."""
    return next(stream, None)


def _close_after_open(task: asyncio.Task[ExecStream]) -> None:
    """Stop a stream that started after its agent was cancelled."""
    if task.cancelled():
        return
    try:
        stream = task.result()
    except Exception:  # noqa: BLE001 - SDK transports may fail while a task is being cancelled.
        return
    stream.close()


class SmolSandbox(PosixShellSandbox):
    """A Strands sandbox with one lazily created Smol microVM.

    Commands run inside the VM. Files transfer through the Smol SDK without
    exposing a host directory. Pass ``MachineConfig(mounts=[...])`` to share a
    local project explicitly. Use ``async with`` or call :meth:`aclose` to delete
    an owned VM; passing ``machine`` borrows that VM and never deletes it.
    """

    def __init__(
        self,
        *,
        config: MachineConfig | None = None,
        conn: ConnectOptions | None = None,
        machine: Machine | None = None,
        working_dir: str = "/workspace",
    ) -> None:
        if not working_dir.startswith("/"):
            raise ValueError("working_dir must be an absolute guest path")
        self.conn = conn or ConnectOptions(target="local")
        self.config = config or MachineConfig(
            image="python:3.12-alpine",
            resources=ResourceSpec(cpus=2, memory_mb=2048, network=False),
            ttl_seconds=3600 if self.conn.target == "cloud" else None,
        )
        self.working_dir = working_dir
        self._machine = machine
        self._owns_machine = False
        self._closed = False
        self._lock = threading.RLock()
        self._streams: set[ExecStream] = set()

    @property
    def machine_id(self) -> str | None:
        """The running machine ID; ``None`` until a new machine is created."""
        with self._lock:
            return self._machine.id if self._machine is not None else None

    def _get_machine(self) -> Machine:
        with self._lock:
            if self._closed:
                raise RuntimeError("Smol sandbox is closed")
            if self._machine is None:
                self._machine = Machine.create(self.config, self.conn)
                self._owns_machine = True
            return self._machine

    async def execute_streaming(
        self,
        command: str,
        *,
        timeout: float | None = None,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> AsyncGenerator[StreamChunk | ExecutionResult, None]:
        """Stream distinct stdout/stderr events and return the command's exit code."""
        if timeout is not None and (not math.isfinite(timeout) or timeout <= 0):
            raise ValueError("timeout must be positive and finite")
        if env:
            validate_env_keys(env)
        machine = await asyncio.to_thread(self._get_machine)
        deadline = time.monotonic() + timeout if timeout is not None else None
        opening = asyncio.create_task(
            asyncio.to_thread(
                machine.exec_stream,
                ["sh", "-c", command],
                # The host deadline reports Strands' typed timeout and preserves
                # partial output. Keep the guest deadline as a safety backstop.
                ExecOptions(
                    workdir=cwd or self.working_dir,
                    timeout=math.ceil(timeout) + 1 if timeout is not None else None,
                    env=env,
                ),
            )
        )
        try:
            if deadline is None:
                stream = await asyncio.shield(opening)
            else:
                stream = await asyncio.wait_for(
                    asyncio.shield(opening), max(0, deadline - time.monotonic())
                )
        except TimeoutError as exc:
            opening.add_done_callback(_close_after_open)
            raise SandboxTimeoutError(timeout, "", "") from exc
        except BaseException:
            # Cancelling a thread wait does not stop the thread: close its stream
            # when it eventually returns, so the command cannot run unseen.
            opening.add_done_callback(_close_after_open)
            raise
        with self._lock:
            if self._closed:
                stream.close()
                raise RuntimeError("Smol sandbox is closed")
            self._streams.add(stream)
        stdout: list[str] = []
        stderr: list[str] = []
        try:
            while True:
                if deadline is None:
                    event = await asyncio.to_thread(_next_event, stream)
                else:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise SandboxTimeoutError(
                            timeout, "".join(stdout), "".join(stderr)
                        )
                    try:
                        event = await asyncio.wait_for(
                            asyncio.to_thread(_next_event, stream), remaining
                        )
                    except TimeoutError as exc:
                        raise SandboxTimeoutError(
                            timeout, "".join(stdout), "".join(stderr)
                        ) from exc
                if event is None:
                    raise RuntimeError("Smol exec stream ended without an exit event")
                kind = event.get("kind")
                if kind in ("stdout", "stderr"):
                    data = str(event["data"])
                    (stdout if kind == "stdout" else stderr).append(data)
                    yield StreamChunk(data=data, stream_type=kind)
                elif kind == "exit":
                    yield ExecutionResult(
                        exit_code=int(event["exit_code"]),
                        stdout="".join(stdout),
                        stderr="".join(stderr),
                    )
                    return
                elif kind == "error":
                    message = str(event.get("message", "Smol command failed"))
                    if timeout is not None and "timed out" in message.lower():
                        raise SandboxTimeoutError(
                            timeout, "".join(stdout), "".join(stderr)
                        )
                    raise OSError(message)
        finally:
            stream.close()
            with self._lock:
                self._streams.discard(stream)

    async def read_file(self, path: str, **kwargs: Any) -> bytes:
        """Read a guest file as bytes through the VM's file API."""
        machine = await asyncio.to_thread(self._get_machine)
        try:
            return await asyncio.to_thread(machine.read_file, path)
        except SmolError as exc:
            if exc.code == "NOT_FOUND":
                raise FileNotFoundError(path) from exc
            raise

    async def write_file(self, path: str, content: bytes, **kwargs: Any) -> None:
        """Write a guest file as bytes through the VM's file API."""
        machine = await asyncio.to_thread(self._get_machine)
        await asyncio.to_thread(machine.write_file, path, content)

    def get_tools(self) -> list[AgentTool]:
        """Vend Strands' standard shell and file editor tools bound to this VM."""
        return [
            make_file_editor(
                sandbox=self,
                name="sandbox_file_editor",
                description=f"{DEFAULT_FILE_EDITOR_DESCRIPTION} Files are in a Smol microVM.",
            ),
            make_shell(
                sandbox=self,
                name="sandbox_shell",
                description=f"{SANDBOX_SHELL_DESCRIPTION} Runs in a Smol microVM at {self.working_dir}.",
            ),
        ]

    def close(self) -> None:
        """Kill active commands and delete an owned VM; leave borrowed VMs intact."""
        with self._lock:
            if self._closed and self._machine is None:
                return
            self._closed = True
            close_error = None
            for stream in tuple(self._streams):
                try:
                    stream.close()
                except Exception as exc:  # noqa: BLE001 - still delete the VM if closing a stream fails.
                    close_error = close_error or exc
            if self._owns_machine and self._machine is not None:
                try:
                    self._machine.delete()
                except SmolError as exc:
                    if exc.code not in {"NOT_FOUND", "VM_NOT_FOUND"}:
                        raise  # A later close() can retry deleting this VM.
            self._machine = None
            self._owns_machine = False
            if close_error is not None:
                raise close_error

    async def aclose(self) -> None:
        """Close without blocking an event loop on VM deletion."""
        await asyncio.to_thread(self.close)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()
