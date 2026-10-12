"""Microsoft Agent Framework shell tool backed by a local or Cloud Smol VM.

Install ``smolmachines[agent-framework]`` to use this optional integration.
Each tool owns one disposable VM unless an existing ``AsyncMachine`` is passed.
Commands use a fresh POSIX shell in that VM; files persist across commands, but
``cd`` and shell variables do not. Use one instance per agent session.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Callable, Mapping
from typing import Literal, TypeVar

from agent_framework import Content, FunctionTool, tool
from agent_framework.tools import ShellCommandError, ShellExecutionError, ShellPolicy, ShellRequest, ShellResult

from .async_machine import AsyncMachine
from .errors import SmolError
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

__all__ = ["SmolShellTool"]

_LOG = logging.getLogger(__name__)
_T = TypeVar("_T")


async def _finish_after_cancellation(task: asyncio.Task[_T]) -> _T:
    """Let a blocking SDK operation settle despite repeated task cancellation."""
    while True:
        try:
            return await asyncio.shield(task)
        except asyncio.CancelledError:
            if task.done():
                return task.result()


_DEFAULT_DESCRIPTION = (
    "Run a POSIX shell command in an isolated Smol VM and return stdout, stderr, "
    "and exit code. Files persist between calls; shell variables and cd do not."
)


class _ResultText(str):
    """Retain structured shell fields while showing plain text to the model."""

    def __new__(cls, result: ShellResult) -> _ResultText:
        value = super().__new__(cls, result.format_for_model())
        value.shell_result = result
        return value


def _parse_result(value: object) -> str | list[Content]:
    if not isinstance(value, _ResultText):
        return str(value)
    result = value.shell_result
    return [
        Content.from_text(
            value,
            additional_properties={
                "stdout": result.stdout,
                "stderr": result.stderr,
                "exit_code": result.exit_code,
                "truncated": result.truncated,
                "timed_out": result.timed_out,
            },
        )
    ]


class _CapturedStream:
    """Remember only the first and last bytes of a guest output stream."""

    def __init__(self, limit: int) -> None:
        self.head_limit = (limit + 1) // 2
        self.tail_limit = limit - self.head_limit
        self.head = bytearray()
        self.tail = bytearray()
        self.total = 0

    def append(self, text: str) -> None:
        data = text.encode("utf-8", "replace")
        self.total += len(data)
        missing = self.head_limit - len(self.head)
        if missing > 0:
            self.head.extend(data[:missing])
        if self.tail_limit:
            self.tail = (self.tail + data)[-self.tail_limit :]

    def view(self, budget: int) -> str:
        if self.total <= budget:
            # The tail may overlap the head for short outputs. Take the
            # remainder from the head exactly once before adding the tail.
            missing = max(0, self.total - len(self.tail))
            data = bytes(self.head[:missing] + self.tail)
        else:
            head = (budget + 1) // 2
            tail = budget - head
            data = bytes(self.head[:head] + (self.tail[-tail:] if tail else b""))
        return data.decode("utf-8", "replace")


def _limit_output(stdout: _CapturedStream, stderr: _CapturedStream, limit: int) -> tuple[str, str, bool]:
    """Keep both streams within the combined byte cap, reserving error space."""
    truncated = stdout.total + stderr.total > limit
    stderr_limit = min(stderr.total, limit // 2)
    stdout_limit = min(stdout.total, limit - stderr_limit)
    stderr_limit = min(stderr.total, limit - stdout_limit)
    return stdout.view(stdout_limit), stderr.view(stderr_limit), truncated


class SmolShellTool:
    """Agent Framework shell executor with a separate Smol VM per instance.

    ``async with SmolShellTool(...)`` creates and owns a disposable local VM by
    default. Pass ``conn=ConnectOptions(target="cloud")`` for Smol Cloud, or
    ``machine=existing_async_machine`` to use a VM managed by the caller. Close
    deletes only VMs created by this tool. Guest networking is off by default;
    use ``MachineConfig(resources=...)`` to opt in to scoped egress.

    ``as_function()`` requires approval by default, matching Agent Framework's
    shell tools; direct ``run()`` calls bypass framework approval. A policy is
    an optional prefilter and does not replace VM isolation or approval.
    """

    def __init__(
        self,
        *,
        machine: AsyncMachine | None = None,
        config: MachineConfig | None = None,
        conn: ConnectOptions | None = None,
        shell: str = "/bin/sh",
        workdir: str = "/workspace",
        env: Mapping[str, str] | None = None,
        timeout: float = 30.0,
        max_output_bytes: int = 64 * 1024,
        policy: ShellPolicy | None = None,
        approval_mode: Literal["always_require", "never_require"] = "always_require",
        on_command: Callable[[str], None] | None = None,
    ) -> None:
        if machine is not None and (config is not None or conn is not None):
            raise ValueError("config and conn apply only when the tool creates its own machine")
        if not shell.startswith("/") or not workdir.startswith("/"):
            raise ValueError("shell and workdir must be absolute guest paths")
        if timeout <= 0 or max_output_bytes <= 0:
            raise ValueError("timeout and max_output_bytes must be greater than zero")
        if approval_mode not in ("always_require", "never_require"):
            raise ValueError("approval_mode must be always_require or never_require")
        self._machine = machine
        self._owned = False
        self._config = config or MachineConfig(
            image="alpine:3.20",
            resources=ResourceSpec(cpus=1, memory_mb=512, network=False, storage_gb=2, overlay_gb=1),
            ttl_seconds=3600 if conn is not None and conn.target == "cloud" else None,
        )
        self._conn = conn
        self._shell = shell
        self._workdir = workdir
        self._env = dict(env or {})
        self._timeout = timeout
        self._max_output_bytes = max_output_bytes
        self._policy = policy or ShellPolicy()
        self._approval_mode = approval_mode
        self._on_command = on_command
        self._lock = asyncio.Lock()

    @property
    def machine(self) -> AsyncMachine | None:
        """The live machine, if started or supplied by the caller."""
        return self._machine

    async def start(self) -> None:
        """Create the disposable VM on first use; do not take ownership of supplied VMs."""
        async with self._lock:
            if self._machine is not None:
                return
            # AsyncMachine.create offloads blocking I/O to a thread. A cancelled
            # await would leave a newly created VM running with no owner.
            task = asyncio.create_task(AsyncMachine.create(self._config, self._conn))
            try:
                machine = await asyncio.shield(task)
            except asyncio.CancelledError:
                machine = await _finish_after_cancellation(task)
                self._machine = machine
                self._owned = True
                await _finish_after_cancellation(asyncio.create_task(machine.delete()))
                self._machine = None
                self._owned = False
                raise
            self._machine = machine
            self._owned = True

    async def close(self) -> None:
        """Delete the machine if this tool created it; safe to call repeatedly."""
        async with self._lock:
            if not self._owned or self._machine is None:
                return
            # Await the SDK's offloaded delete before releasing ownership, even
            # if the agent run is cancelled while the VM is shutting down.
            task = asyncio.create_task(self._machine.delete())
            cancelled = False
            try:
                await asyncio.shield(task)
            except asyncio.CancelledError:
                cancelled = True
                await _finish_after_cancellation(task)
            self._machine = None
            self._owned = False
            if cancelled:
                raise asyncio.CancelledError()

    async def __aenter__(self) -> SmolShellTool:
        await self.start()
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()

    async def run(self, command: str, *, timeout: float | None = None) -> ShellResult:
        """Run a command in the guest, with the timeout enforced by SmolVM."""
        if not command or not command.strip():
            raise ShellCommandError("Command must be non-empty")
        decision = self._policy.evaluate(ShellRequest(command=command, workdir=self._workdir))
        if decision.decision == "deny":
            raise ShellCommandError(f"Command rejected by policy: {decision.reason}")
        effective_timeout = self._timeout if timeout is None else timeout
        if effective_timeout <= 0:
            raise ValueError("timeout must be greater than zero")
        if self._on_command is not None:
            try:
                self._on_command(command)
            except Exception:
                _LOG.exception("on_command hook raised")
        if self._machine is None:
            await self.start()
        async with self._lock:
            if self._machine is None:
                raise RuntimeError("Smol VM is closed")
            started = time.monotonic()
            # The SDK yields events as they arrive, so a noisy guest cannot
            # allocate its entire output in the agent process before the cap.
            stdout = _CapturedStream(self._max_output_bytes)
            stderr = _CapturedStream(self._max_output_bytes)
            exit_code: int | None = None

            async def read_stream() -> None:
                nonlocal exit_code
                # The inner shell closes stdin on local releases that leave
                # streamed-exec stdin open, then replaces itself with the
                # requested shell without changing its exit status.
                argv = ["/bin/sh", "-c", 'exec "$@" </dev/null', "sh", self._shell, "-c", command]
                async for event in self._machine.exec_stream(
                    argv, ExecOptions(env=self._env or None, workdir=self._workdir, timeout=effective_timeout)
                ):
                    kind = event.get("kind")
                    if kind == "stdout" or kind == "stderr":
                        data = event.get("data")
                        if not isinstance(data, str):
                            raise ShellExecutionError("Smol VM returned invalid command output")
                        (stdout if kind == "stdout" else stderr).append(data)
                    elif kind == "exit":
                        code = event.get("exit_code")
                        if type(code) is not int:
                            raise ShellExecutionError("Smol VM returned an invalid exit status")
                        exit_code = code
                    elif kind == "error":
                        raise ShellExecutionError("Smol VM command stream failed")
                    else:
                        raise ShellExecutionError("Smol VM returned an unknown command event")

            timed_out = False
            task = asyncio.create_task(asyncio.wait_for(read_stream(), timeout=effective_timeout))
            try:
                await asyncio.shield(task)
            except asyncio.CancelledError:
                # Keep the activity alive until the guest deadline or exit,
                # including on repeated cancellation, as for buffered exec.
                try:
                    await _finish_after_cancellation(task)
                finally:
                    raise
            except asyncio.TimeoutError:
                timed_out = True
                exit_code = 124
            except SmolError as error:
                raise ShellExecutionError(f"Smol VM exec failed ({error.code}): {error}") from error
            if exit_code is None:
                raise ShellExecutionError("Smol VM command stream ended without an exit status")
            out, err, truncated = _limit_output(stdout, stderr, self._max_output_bytes)
            return ShellResult(
                stdout=out,
                stderr=err,
                exit_code=exit_code,
                duration_ms=round((time.monotonic() - started) * 1000),
                truncated=truncated,
                timed_out=timed_out or (exit_code == 124 and "timed out" in err.lower()),
            )

    def as_function(
        self, *, name: str = "run_smol_shell", description: str | None = None
    ) -> FunctionTool:
        """Expose a provider-compatible shell function with framework approval."""

        async def _run_shell(command: str) -> str:
            try:
                result = await self.run(command)
            except ShellCommandError as exc:
                return str(exc)
            return _ResultText(result)

        effective_description = description or _DEFAULT_DESCRIPTION
        _run_shell.__doc__ = effective_description
        return tool(
            func=_run_shell,
            name=name,
            description=effective_description,
            approval_mode=self._approval_mode,
            kind="shell",
            result_parser=_parse_result,
        )
