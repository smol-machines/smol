"""A Smol Machines microVM behind Pydantic AI's workspace protocol."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import AsyncGenerator, AsyncIterator, Mapping
from contextlib import aclosing
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Literal

import anyio
from pydantic_ai.exceptions import UserError
from pydantic_ai.workspaces import (
    CommandResult,
    SupportsCommands,
    WorkspaceBackend,
    WorkspaceError,
    WorkspaceOutputLimitError,
    WorkspaceRef,
    WorkspaceTimeoutError,
    WorkspaceUnavailableError,
)

from smol.pydantic_ai_harness._helpers import (
    absolute_path,
    check_timeout,
    command_argv,
    running_on_asyncio,
    safe_credential_reason,
)

if TYPE_CHECKING:
    from pydantic_ai.workspaces import WorkspaceCommand

try:
    from smol import (
        AsyncMachine,
        ConnectOptions,
        ExecOptions,
        MachineConfig,
        ResourceSpec,
        SmolError,
    )
except ImportError as exc:
    raise ImportError(
        "Install `smolmachines[pydantic-ai-harness]` to use SmolSandbox."
    ) from exc

__all__ = ("SmolSandboxBackend",)

logger = logging.getLogger(__name__)
_MAX_OUTPUT_BYTES = 10 * 1024 * 1024
_OUTPUT_PREVIEW_CHARS = 64 * 1024


def _connection(
    target: Literal["local", "cloud"], base_url: str | None, api_key: str | None
) -> ConnectOptions:
    return ConnectOptions(target=target, base_url=base_url, api_key=api_key)


def _sdk_error(error: SmolError, operation: str) -> Exception:
    if error.code == "NOT_FOUND":
        return WorkspaceUnavailableError(f"{operation}: machine no longer exists")
    if error.code in {"UNAUTHORIZED", "FORBIDDEN", "AUTH"}:
        return WorkspaceUnavailableError(
            f"{operation}: {safe_credential_reason(error)}"
        )
    if error.code in {"INVALID_CONFIG", "NOT_SUPPORTED"}:
        return WorkspaceUnavailableError(f"{operation}: {error}")
    return error


def _empty_text() -> list[str]:
    return []


@dataclass
class _CommandOutput:
    stdout_parts: list[str] = field(default_factory=_empty_text)
    stderr_parts: list[str] = field(default_factory=_empty_text)
    total_bytes: int = 0
    exit_code: int | None = None

    def preview(self) -> tuple[str, str]:
        return "".join(self.stdout_parts)[:_OUTPUT_PREVIEW_CHARS], "".join(
            self.stderr_parts
        )[:_OUTPUT_PREVIEW_CHARS]

    def push(self, event: Mapping[str, object]) -> None:
        kind = event.get("kind")
        if kind in ("stdout", "stderr"):
            data = event.get("data")
            if not isinstance(data, str):
                raise WorkspaceError(f"Smol command returned invalid {kind} output")
            self.total_bytes += len(data.encode("utf-8"))
            if self.total_bytes > _MAX_OUTPUT_BYTES:
                stdout, stderr = self.preview()
                raise WorkspaceOutputLimitError(
                    "Smol command output exceeded the 10 MiB workspace limit",
                    limit=_MAX_OUTPUT_BYTES,
                    stdout=stdout,
                    stderr=stderr,
                )
            (self.stdout_parts if kind == "stdout" else self.stderr_parts).append(data)
        elif kind == "exit":
            code = event.get("exit_code")
            if type(code) is not int:
                raise WorkspaceError("Smol command returned an invalid exit code")
            self.exit_code = code
        elif kind == "error":
            message = event.get("message")
            raise SmolError(
                "SMOLVM_ERROR",
                message if isinstance(message, str) else "command failed",
            )
        else:
            raise WorkspaceError("Smol command returned an unknown event")

    def result(self) -> CommandResult:
        if self.exit_code is None:
            raise WorkspaceError("Smol command stream ended without an exit status")
        return CommandResult(
            exit_code=self.exit_code,
            stdout="".join(self.stdout_parts),
            stderr="".join(self.stderr_parts),
        )


class SmolSandboxBackend(WorkspaceBackend, SupportsCommands):
    """Lazy Smol VM workspace using the same command interface on a local host or Smol Cloud.

    A created VM is persistent locally so its `WorkspaceRef` can be reattached on a later run.
    The caller owns deletion; this backend never deletes a VM on run exit.
    Filesystem operations use Pydantic AI's POSIX shell fallback, which requires basic BusyBox
    utilities in custom images as well as the default Alpine image.
    """

    def __init__(
        self,
        *,
        ref: WorkspaceRef | None = None,
        target: Literal["local", "cloud"] = "local",
        image: str = "alpine:3.20",
        memory_mb: int = 1024,
        network: bool = False,
        working_dir: str = "/workspace",
        env: Mapping[str, str] | None = None,
        api_key: str | None = None,
        base_url: str | None = None,
        ttl_seconds: int = 3600,
        ready_timeout: float = 120,
    ) -> None:
        if ref is not None and ref.provider != "smol":
            raise ValueError(
                f"unsupported workspace provider {ref.provider!r}; expected 'smol'"
            )
        if target == "local":
            self._target: Literal["local", "cloud"] = "local"
        elif target == "cloud":
            self._target = "cloud"
        else:
            raise ValueError("target must be 'local' or 'cloud'")
        if memory_mb < 256 or type(memory_mb) is not int:
            raise ValueError("memory_mb must be an integer of at least 256")
        if type(ttl_seconds) is not int or ttl_seconds < 1:
            raise ValueError("ttl_seconds must be a positive integer")
        check_timeout(ready_timeout)
        if env is not None and any(
            type(k) is not str or type(v) is not str for k, v in env.items()
        ):
            raise TypeError("env keys and values must be strings")
        self._ref = ref
        self._machine: AsyncMachine | None = None
        self._acquisition: asyncio.Task[AsyncMachine] | None = None
        self._lock = anyio.Lock()
        self._resolved_dir: str | None = None
        self._image = image
        self._memory_mb = memory_mb
        self._network = network
        self._working_dir = absolute_path("working_dir", working_dir)
        assert self._working_dir is not None
        self._env = dict(env) if env is not None else None
        self._api_key = api_key
        self._base_url = base_url
        self._ttl_seconds = ttl_seconds
        self._ready_timeout = ready_timeout

    @property
    def ref(self) -> WorkspaceRef | None:
        """VM identity, retained after creation even if setup or the run fails."""
        return self._ref

    def _conn(self) -> ConnectOptions:
        return _connection(self._target, self._base_url, self._api_key)

    async def get_machine(self) -> AsyncMachine:
        """Create or attach once, returning the SDK handle for operations outside the workspace protocol."""
        if not running_on_asyncio():
            raise UserError(
                "SmolSandbox needs asyncio: the Smol SDK runs blocking calls on asyncio threads."
            )
        if self._ref is None:
            task = self._acquisition
            if task is None:
                task = asyncio.create_task(
                    self._acquire_detached(), name="smol-sandbox-acquisition"
                )
                task.add_done_callback(
                    lambda done: done.cancelled() or done.exception()
                )
                self._acquisition = task
            await asyncio.wait([task])
            return task.result()
        return await self._acquire()

    async def _acquire_detached(self) -> AsyncMachine:
        try:
            return await self._acquire()
        finally:
            self._acquisition = None

    async def _acquire(self) -> AsyncMachine:
        async with self._lock:
            if self._machine is None:
                try:
                    if self._ref is not None:
                        machine = await AsyncMachine.connect(self._ref.id, self._conn())
                        await machine.wait_until_ready(timeout_s=self._ready_timeout)
                    else:
                        machine = await AsyncMachine.create(
                            MachineConfig(
                                image=self._image,
                                persistent=True,
                                resources=ResourceSpec(
                                    memory_mb=self._memory_mb, network=self._network
                                ),
                                workdir=self._working_dir,
                                env=self._env,
                                ttl_seconds=self._ttl_seconds
                                if self._target == "cloud"
                                else None,
                                ready_timeout_seconds=self._ready_timeout,
                            ),
                            self._conn(),
                        )
                        self._ref = WorkspaceRef(provider="smol", id=machine.id)
                        logger.info("Created Smol machine %s", machine.id)
                    self._machine = machine
                except SmolError as error:
                    translated = _sdk_error(error, "Acquire Smol machine")
                    if translated is error:
                        raise
                    raise translated from error
        return self._machine

    async def working_dir(self) -> str:
        """Return the canonical path command executions use inside the VM."""
        if self._resolved_dir is None:
            result = await self.run(["pwd", "-P"])
            if result.exit_code != 0:
                raise WorkspaceUnavailableError(
                    f"Cannot resolve Smol working directory: {result.stderr}"
                )
            self._resolved_dir = result.stdout.strip()
        return self._resolved_dir

    async def run(
        self,
        command: WorkspaceCommand,
        *,
        shell: bool = False,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
    ) -> CommandResult:
        """Run one command; return its exit code and complete output, or a typed limit error."""
        argv = command_argv(command, shell)
        check_timeout(timeout)
        if env is not None and any(
            type(k) is not str or type(v) is not str for k, v in env.items()
        ):
            raise TypeError("env keys and values must be strings")
        machine = await self.get_machine()
        combined_env = {**(self._env or {}), **(env or {})}
        # Keep stdin at EOF on older SmolVM releases, where streamed exec could
        # leave `read` waiting. The shell also maps a missing executable to 127.
        # `exec` preserves argv and replaces the shell with the requested process.
        argv = ["/bin/sh", "-c", 'exec "$@" </dev/null', "sh", *argv]
        output = _CommandOutput()
        try:
            with anyio.fail_after(timeout):
                stream: AsyncIterator[dict[str, object]] = machine.exec_stream(  # pyright: ignore[reportUnknownMemberType, reportUnknownVariableType]
                    argv,
                    ExecOptions(
                        env=combined_env or None,
                        workdir=self._working_dir,
                        # Keep a guest deadline if the Cloud caller disappears.
                        timeout=timeout + 1 if timeout is not None else None,
                    ),
                )
                if not isinstance(stream, AsyncGenerator):
                    raise WorkspaceError(
                        "Smol command stream cannot be closed on cancellation"
                    )
                typed_stream: AsyncGenerator[dict[str, object], None] = stream  # pyright: ignore[reportUnknownVariableType]
                async with aclosing(typed_stream) as events:
                    async for event in events:
                        output.push(event)
        except TimeoutError as error:
            stdout, stderr = output.preview()
            raise WorkspaceTimeoutError(
                f"Command timed out after {timeout:g} seconds",
                stdout=stdout,
                stderr=stderr,
            ) from error
        except SmolError as error:
            translated = _sdk_error(error, "Execute Smol command")
            if translated is error and error.code in {
                "SMOLVM_ERROR",
                "NETWORK_ERROR",
                "TIMEOUT",
            }:
                # A deleted VM can close its command socket before the SDK's local
                # state lookup reports stopped. Retry only the bounded liveness probe;
                # a live VM's transport errors remain transient SDK errors.
                for attempt in range(6):
                    try:
                        state = await machine.state()
                    except SmolError as probe_error:
                        if probe_error.code == "NOT_FOUND":
                            raise WorkspaceUnavailableError(
                                "Execute Smol command: machine no longer exists"
                            ) from error
                        break
                    if state in {"stopped", "deleted", "error", "failed"}:
                        raise WorkspaceUnavailableError(
                            f"Execute Smol command: machine is {state}"
                        ) from error
                    if (
                        attempt < 5
                        and self._target == "local"
                        and "connection closed" in str(error).lower()
                    ):
                        await anyio.sleep(0.1)
                    else:
                        break
            if translated is error:
                raise
            raise translated from error
        return output.result()


async def destroy_machine(
    ref: WorkspaceRef,
    *,
    target: Literal["local", "cloud"],
    base_url: str | None,
    api_key: str | None,
) -> None:
    """Delete a VM by its ref; attach mode must never create a replacement."""
    if ref.provider != "smol":
        raise ValueError(
            f"unsupported workspace provider {ref.provider!r}; expected 'smol'"
        )
    if not running_on_asyncio():
        raise UserError(
            "SmolSandbox needs asyncio: the Smol SDK runs blocking calls on asyncio threads."
        )
    try:
        machine = await AsyncMachine.connect(
            ref.id, _connection(target, base_url, api_key)
        )
        await machine.delete()
    except SmolError as error:
        translated = _sdk_error(error, "Delete Smol machine")
        if translated is error:
            raise
        raise translated from error
