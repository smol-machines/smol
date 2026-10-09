"""A microVM-backed shell tool for the OpenAI Agents SDK.

The application owns the machine's lifetime and network policy. The agent sees
only the command tool; it never receives a cloud token or host shell access.
"""

from __future__ import annotations

from .async_machine import AsyncMachine
from .types import ExecOptions


def create_smol_shell_tool(
    machine: AsyncMachine,
    *,
    timeout: float = 60,
    max_output_chars: int = 12_000,
):
    """Return an Agents SDK function tool that runs shell commands in ``machine``.

    Create one machine per trust boundary and delete it when the agent is done.
    Output is streamed and capped in the tool response, while execution continues
    to completion so output truncation does not change command behavior.
    """
    try:
        from agents import function_tool
    except ImportError as exc:
        raise ImportError(
            "Install smolmachines[openai-agents] to use create_smol_shell_tool"
        ) from exc

    if timeout <= 0:
        raise ValueError("timeout must be positive")
    if max_output_chars <= 0:
        raise ValueError("max_output_chars must be positive")

    @function_tool(name_override="smol_shell", failure_error_function=None)
    async def smol_shell(command: str) -> str:
        """Run a shell command inside the isolated Smol machine; return output and exit code."""
        output: list[str] = []
        remaining = max_output_chars
        truncated = False
        exit_code: int | None = None
        async for event in machine.exec_stream(
            ["sh", "-c", command], ExecOptions(timeout=timeout)
        ):
            kind = event.get("kind")
            if kind in ("stdout", "stderr"):
                chunk = event.get("data", "")
                prefix = "stderr: " if kind == "stderr" else ""
                text = prefix + chunk
                if len(text) > remaining:
                    truncated = True
                if remaining:
                    output.append(text[:remaining])
                    remaining -= min(len(text), remaining)
            elif kind == "exit":
                exit_code = event["exit_code"]
            elif kind == "error":
                raise RuntimeError("Smol command failed: " + event.get("message", "unknown error"))

        if exit_code is None:
            raise RuntimeError("Smol command stream ended without an exit code")
        result = "".join(output)
        if truncated:
            result += "\n[output truncated]"
        return f"{result}\n[exit code: {exit_code}]"

    return smol_shell
