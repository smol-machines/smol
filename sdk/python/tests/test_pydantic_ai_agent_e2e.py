"""Opt-in real VM smoke test for the Pydantic AI Harness integration."""

from __future__ import annotations

import os

import pytest
from pydantic_ai import Agent, RunContext
from pydantic_ai.models.test import TestModel
from pydantic_ai.workspaces import WorkspaceRef
from pydantic_ai_harness.filesystem import FileSystem, FileSystemToolset
from pydantic_ai_harness.shell import Shell
from smol.pydantic_ai_harness import SmolSandbox, SmolSandboxBackend

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


async def test_cloud_agent_tools_survive_pause_resume_and_reattachment() -> None:
    if os.getenv("SMOL_SANDBOX_CLOUD_LIVE") != "1":
        pytest.skip("set SMOL_SANDBOX_CLOUD_LIVE=1 after `smol auth login`")

    sandbox = SmolSandbox(
        target="cloud",
        image="alpine:3.20",
        memory_mb=768,
        network=False,
        branchable=True,
        ttl_seconds=300,
    )
    backend = SmolSandboxBackend(
        target="cloud",
        image="alpine:3.20",
        memory_mb=768,
        network=False,
        branchable=True,
        ttl_seconds=300,
    )
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
            "harness-cloud.txt", "from cloud guest", workspace=ctx.workspace
        )
        command = await shell.get_toolset().run_command(ctx, "cat harness-cloud.txt")
        content = await toolset.read_file("harness-cloud.txt", workspace=ctx.workspace)
        assert "from cloud guest" in command
        return content

    try:
        result = await agent.run(
            "Create and read a file inside the Cloud VM.", workspace=backend
        )
        assert "from cloud guest" in result.output
        ref = result.workspace.ref
        assert ref is not None and ref == backend.ref
        machine = await backend.get_machine()
        await machine.pause()
        await machine.resume()
        assert (
            await sandbox.backend(ref).run(["cat", "/workspace/harness-cloud.txt"])
        ).stdout == "from cloud guest"
    finally:
        if backend.ref is not None:
            await sandbox.destroy(backend.ref)


async def test_branchable_agent_workspace_is_independent() -> None:
    if os.getenv("SMOL_SANDBOX_LIVE") != "1":
        pytest.skip(
            "set SMOL_SANDBOX_LIVE=1 with local virtualization to run the VM test"
        )

    from uuid import uuid4

    sandbox = SmolSandbox(branchable=True)
    backend = SmolSandboxBackend(branchable=True)
    source = await backend.get_machine()
    child = None
    try:
        initial = await backend.run(
            ["sh", "-c", "printf source > /workspace/branch-test.txt"]
        )
        assert initial.exit_code == 0
        child = await source.branch(f"harness-branch-{uuid4().hex[:8]}")
        child_ref = WorkspaceRef(provider="smol", id=child.id)

        agent = Agent(TestModel(call_tools=["inspect_branch"]), capabilities=[sandbox])

        @agent.tool
        async def inspect_branch(ctx: RunContext[object]) -> str:
            before = await ctx.workspace.run(["cat", "/workspace/branch-test.txt"])
            assert before.stdout == "source"
            changed = await ctx.workspace.run(
                ["sh", "-c", "printf child > /workspace/branch-test.txt"]
            )
            assert changed.exit_code == 0
            return (
                await ctx.workspace.run(["cat", "/workspace/branch-test.txt"])
            ).stdout

        result = await agent.run(
            "Try a different change in this branch.", workspace=child_ref
        )
        assert "child" in result.output
        assert (
            await backend.run(["cat", "/workspace/branch-test.txt"])
        ).stdout == "source"
    finally:
        if child is not None:
            await child.delete()
        await source.delete()
