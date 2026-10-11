"""Run Agent Framework CodeAct Python in a disposable local or Cloud Smol VM.

The provider owns one VM per instance. Use it as an async context manager for
one agent session; Python processes are fresh per call and share a persistent
filesystem, while code never runs inside the agent's host process.
"""

from __future__ import annotations

import shlex
from typing import Any, Literal

from agent_framework import ContextProvider, FunctionTool, tool
from agent_framework.tools import ShellResult

from .agent_framework import SmolShellTool, _ResultText, _parse_result
from .async_machine import AsyncMachine
from .types import ConnectOptions, MachineConfig, ResourceSpec

__all__ = ["SmolCodeActProvider", "SmolExecuteCodeTool"]

_INSTRUCTIONS = (
    "Use execute_code(code=...) to run Python 3 in an isolated Linux VM. "
    "Print values you want to see. /workspace files persist between calls, "
    "but Python variables and imports do not. Subprocesses and installed "
    "packages are available inside the VM. Network access is disabled by "
    "default and must be enabled by the application."
)


class SmolExecuteCodeTool:
    """CodeAct execute_code tool backed by a VM; close it after the agent session."""

    def __init__(
        self,
        *,
        machine: AsyncMachine | None = None,
        config: MachineConfig | None = None,
        conn: ConnectOptions | None = None,
        timeout: float = 30.0,
        max_output_bytes: int = 64 * 1024,
        approval_mode: Literal["always_require", "never_require"] = "never_require",
    ) -> None:
        if machine is None and config is None:
            config = MachineConfig(
                image="python:3.12-alpine",
                resources=ResourceSpec(cpus=1, memory_mb=512, network=False, storage_gb=2, overlay_gb=1),
                ttl_seconds=3600 if conn is not None and conn.target == "cloud" else None,
            )
        self._shell = SmolShellTool(
            machine=machine,
            config=config,
            conn=conn,
            timeout=timeout,
            max_output_bytes=max_output_bytes,
            approval_mode=approval_mode,
        )
        self._approval_mode = approval_mode

    @property
    def machine(self) -> AsyncMachine | None:
        """The active machine, if started or supplied by the caller."""
        return self._shell.machine

    async def start(self) -> None:
        await self._shell.start()

    async def close(self) -> None:
        await self._shell.close()

    async def __aenter__(self) -> SmolExecuteCodeTool:
        await self.start()
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()

    async def run(self, code: str, *, timeout: float | None = None) -> ShellResult:
        """Run code in a fresh Python process on the same VM filesystem."""
        if not code or not code.strip():
            raise ValueError("code must be non-empty")
        return await self._shell.run("python3 -c " + shlex.quote(code), timeout=timeout)

    def as_function(self) -> FunctionTool:
        """Expose an approval-aware execute_code tool with structured output."""

        async def execute_code(code: str) -> str:
            """Execute Python 3 in an isolated Linux VM; print values to return them."""
            return _ResultText(await self.run(code))

        return tool(
            func=execute_code,
            name="execute_code",
            approval_mode=self._approval_mode,
            result_parser=_parse_result,
        )


class SmolCodeActProvider(ContextProvider):
    """Inject a VM-backed CodeAct tool and instructions into each agent run.

    One provider serves one agent session; ``async with`` deletes its VM even
    when the agent run raises. Host tool callbacks are not exposed to the VM.
    """

    def __init__(
        self,
        source_id: str = "smol_codeact",
        *,
        machine: AsyncMachine | None = None,
        config: MachineConfig | None = None,
        conn: ConnectOptions | None = None,
        timeout: float = 30.0,
        max_output_bytes: int = 64 * 1024,
        approval_mode: Literal["always_require", "never_require"] = "never_require",
    ) -> None:
        super().__init__(source_id)
        self._code_tool = SmolExecuteCodeTool(
            machine=machine,
            config=config,
            conn=conn,
            timeout=timeout,
            max_output_bytes=max_output_bytes,
            approval_mode=approval_mode,
        )

    @property
    def execute_code_tool(self) -> SmolExecuteCodeTool:
        """The provider's VM-backed tool for direct invocation or inspection."""
        return self._code_tool

    async def start(self) -> None:
        """Start the session VM before its first agent run."""
        await self._code_tool.start()

    async def close(self) -> None:
        """Delete this provider's VM, if it owns one."""
        await self._code_tool.close()

    async def __aenter__(self) -> SmolCodeActProvider:
        await self.start()
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()

    async def before_run(
        self,
        *,
        agent: Any,
        session: Any,
        context: Any,
        state: dict[str, Any],
    ) -> None:
        """Register execute_code and the guest's capabilities for this run."""
        context.extend_instructions(self.source_id, _INSTRUCTIONS)
        context.extend_tools(self.source_id, [self._code_tool.as_function()])
