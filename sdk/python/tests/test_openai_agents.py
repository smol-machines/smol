"""Contract tests for the OpenAI Agents SDK shell tool (optional extra)."""

import asyncio
import json

import pytest

pytest.importorskip("agents")
from agents.tool_context import ToolContext  # noqa: E402

from smol.openai_agents import create_smol_shell_tool  # noqa: E402


class FakeMachine:
    def __init__(self, events):
        self.events = events
        self.calls = []

    async def exec_stream(self, command, options):
        self.calls.append((command, options))
        for event in self.events:
            yield event


async def invoke(tool, command):
    arguments = json.dumps({"command": command})
    ctx = ToolContext(
        context=None,
        tool_name=tool.name,
        tool_call_id="test-call",
        tool_arguments=arguments,
    )
    return await tool.on_invoke_tool(ctx, arguments)


def test_executes_inside_provided_machine_and_reports_exit_status():
    async def scenario():
        vm = FakeMachine(
            [
                {"kind": "stdout", "data": "work\n"},
                {"kind": "stderr", "data": "warning\n"},
                {"kind": "exit", "exit_code": 7},
            ]
        )
        tool = create_smol_shell_tool(vm, timeout=4)
        output = await invoke(tool, "echo work")
        assert tool.name == "smol_shell"
        assert vm.calls[0][0] == ["sh", "-c", "echo work"]
        assert vm.calls[0][1].timeout == 4
        assert "work\n" in output
        assert "stderr: warning\n" in output
        assert "[exit code: 7]" in output

    asyncio.run(scenario())


def test_output_cap_does_not_cancel_command():
    async def scenario():
        vm = FakeMachine(
            [
                {"kind": "stdout", "data": "abcdef"},
                {"kind": "stdout", "data": "more output"},
                {"kind": "exit", "exit_code": 0},
            ]
        )
        result = await invoke(create_smol_shell_tool(vm, max_output_chars=6), "true")
        assert result == "abcdef\n[output truncated]\n[exit code: 0]"
        assert len(vm.calls) == 1

    asyncio.run(scenario())


def test_missing_exit_rejected():
    async def scenario():
        vm = FakeMachine([{"kind": "stdout", "data": "partial"}])
        tool = create_smol_shell_tool(vm)
        with pytest.raises(RuntimeError, match="without an exit code"):
            await invoke(tool, "exit 1")

    asyncio.run(scenario())


@pytest.mark.parametrize("kwargs", [{"timeout": 0}, {"max_output_chars": -1}])
def test_invalid_limits(kwargs):
    with pytest.raises(ValueError):
        create_smol_shell_tool(FakeMachine([]), **kwargs)
