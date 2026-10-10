"""Opt-in real VM smoke test for the Pydantic AI Harness integration."""

from __future__ import annotations

import os

import pytest
from pydantic_ai import Agent, RunContext
from pydantic_ai.models.test import TestModel
from pydantic_ai.workspaces import WorkspaceRef
from pydantic_ai_harness.filesystem import FileSystem, FileSystemToolset
from pydantic_ai_harness.shell import Shell
from smol.pydantic_ai_harness import SmolSandbox

pytestmark = pytest.mark.anyio


async def test_agent_uses_smol_vm_and_reattaches() -> None:
    if os.getenv("SMOL_SANDBOX_LIVE") != "1":
        pytest.skip(
            "set SMOL_SANDBOX_LIVE=1 with local virtualization to run the VM test"
        )

    sandbox = SmolSandbox()
    shell = Shell()
    files = FileSystem()
    agent = Agent(
        TestModel(call_tools=["write_and_read"]), capabilities=[sandbox, shell, files]
    )

    @agent.tool
    async def write_and_read(ctx: RunContext[object]) -> str:
        toolset = files.get_toolset()
        assert isinstance(toolset, FileSystemToolset)
        await toolset.write_file(
            "harness-test.txt", "from guest", workspace=ctx.workspace
        )
        command = await shell.get_toolset().run_command(ctx, "cat harness-test.txt")
        content = await toolset.read_file("harness-test.txt", workspace=ctx.workspace)
        assert "from guest" in command
        assert "from guest" in content
        return content

    ref: WorkspaceRef | None = None
    try:
        result = await agent.run("Create and read a file inside the VM.")
        assert "from guest" in result.output
        ref = result.workspace.ref
        assert ref is not None and ref.provider == "smol"
        attached = sandbox.backend(ref)
        assert (
            await attached.run(["cat", "/workspace/harness-test.txt"])
        ).stdout == "from guest"
    finally:
        if ref is not None:
            await sandbox.destroy(ref)
