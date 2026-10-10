"""Opt-in real VM smoke test for the Pydantic AI Harness integration."""

from __future__ import annotations

import os

import pytest
from pydantic_ai import Agent, RunContext
from pydantic_ai.models.test import TestModel
from pydantic_ai.workspaces import WorkspaceRef
from pydantic_ai_harness.filesystem import FileSystem
from pydantic_ai_harness.shell import Shell
from smol.pydantic_ai_harness import SmolSandbox

pytestmark = pytest.mark.anyio


async def test_agent_uses_smol_vm_and_reattaches() -> None:
    if os.getenv("SMOL_SANDBOX_LIVE") != "1":
        pytest.skip(
            "set SMOL_SANDBOX_LIVE=1 with local virtualization to run the VM test"
        )

    sandbox = SmolSandbox()
    agent = Agent(
        TestModel(call_tools=["write_and_read"]),
        capabilities=[sandbox, Shell(), FileSystem()],
    )

    @agent.tool
    async def write_and_read(ctx: RunContext[object]) -> str:
        await ctx.workspace.write_bytes("/workspace/harness-test.txt", b"from guest")
        result = await ctx.workspace.run(["cat", "/workspace/harness-test.txt"])
        assert result.exit_code == 0
        return result.stdout

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
