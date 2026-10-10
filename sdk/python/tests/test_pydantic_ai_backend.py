"""SDK boundary and lifecycle tests that do not require a hypervisor."""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from typing import TYPE_CHECKING

import pytest
from pydantic_ai import RunContext
from pydantic_ai.workspaces import (
    WorkspaceOutputLimitError,
    WorkspaceRef,
    WorkspaceUnavailableError,
)
from smol import AsyncMachine, SmolError
from smol.pydantic_ai_harness import SmolSandbox, SmolSandboxBackend, _backend

if TYPE_CHECKING:
    from smol import ConnectOptions, ExecOptions, MachineConfig

pytestmark = pytest.mark.anyio


class FakeMachine:
    id = "test-machine"

    def __init__(self) -> None:
        self.commands: list[tuple[list[str], ExecOptions | None]] = []
        self.deleted = False

    async def exec_stream(
        self, command: list[str], opts: ExecOptions | None = None
    ) -> AsyncIterator[dict[str, object]]:
        self.commands.append((command, opts))
        if command == ["/bin/sh", "-c", 'exec "$@" </dev/null', "sh", "pwd", "-P"]:
            yield {"kind": "stdout", "data": "/workspace\n"}
            yield {"kind": "exit", "exit_code": 0}
        else:
            yield {"kind": "stdout", "data": "out"}
            yield {"kind": "stderr", "data": "err"}
            yield {"kind": "exit", "exit_code": 7}

    async def wait_until_ready(
        self, timeout_s: float = 120, interval_s: float = 1
    ) -> None:
        del timeout_s, interval_s

    async def delete(self, include_usage: bool = False) -> None:
        del include_usage
        self.deleted = True


@pytest.fixture
async def machine_sdk(
    monkeypatch: pytest.MonkeyPatch,
) -> AsyncIterator[tuple[FakeMachine, list[MachineConfig]]]:
    machine = FakeMachine()
    configurations: list[MachineConfig] = []

    async def create(config: MachineConfig, conn: ConnectOptions) -> FakeMachine:
        configurations.append(config)
        assert conn.target == "local"
        return machine

    async def connect(machine_id: str, conn: ConnectOptions) -> FakeMachine:
        assert machine_id == machine.id and conn.target == "local"
        return machine

    monkeypatch.setattr(AsyncMachine, "create", create)
    monkeypatch.setattr(AsyncMachine, "connect", connect)
    yield machine, configurations


async def test_lazy_single_creation_and_reattach(
    machine_sdk: tuple[FakeMachine, list[MachineConfig]],
) -> None:
    machine, configs = machine_sdk
    backend = SmolSandboxBackend(env={"BASE": "value"}, branchable=True)
    assert backend.ref is None and configs == []
    first, second = await asyncio.gather(backend.get_machine(), backend.get_machine())
    assert first is second is machine and len(configs) == 1
    assert configs[0].persistent and configs[0].image == "alpine:3.20"
    assert configs[0].branchable is True
    ref = backend.ref
    assert ref == WorkspaceRef(provider="smol", id=machine.id)
    assert ref is not None
    attached = SmolSandboxBackend(ref=ref)
    assert await attached.get_machine() is machine
    result = await backend.run(["echo", "$()"], env={"RUN": "yes"})
    assert (result.exit_code, result.stdout, result.stderr) == (7, "out", "err")
    argv, opts = machine.commands[-1]
    assert argv == ["/bin/sh", "-c", 'exec "$@" </dev/null', "sh", "echo", "$()"]
    assert (
        opts is not None
        and opts.env == {"BASE": "value", "RUN": "yes"}
        and opts.workdir == "/workspace"
    )
    assert await backend.working_dir() == "/workspace"
    await SmolSandbox[None]().destroy(ref)
    assert machine.deleted


async def test_cancelled_create_still_retains_ref(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    ready = asyncio.Event()
    release = asyncio.Event()
    created: list[FakeMachine] = []
    machine = FakeMachine()

    async def create(config: MachineConfig, conn: ConnectOptions) -> FakeMachine:
        del config, conn
        ready.set()
        await release.wait()
        created.append(machine)
        return machine

    monkeypatch.setattr(AsyncMachine, "create", create)
    backend = SmolSandboxBackend()
    task = asyncio.create_task(backend.get_machine())
    await ready.wait()
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    release.set()
    for _ in range(100):
        if backend.ref is not None:
            break
        await asyncio.sleep(0)
    assert backend.ref == WorkspaceRef(provider="smol", id=machine.id)
    assert len(created) == 1
    assert await backend.get_machine() is machine


async def test_missing_attached_vm_is_never_recreated(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def connect(machine_id: str, conn: ConnectOptions) -> FakeMachine:
        del machine_id, conn
        raise SmolError("NOT_FOUND", "secret URL")

    monkeypatch.setattr(AsyncMachine, "connect", connect)
    backend = SmolSandboxBackend(ref=WorkspaceRef(provider="smol", id="deleted"))
    with pytest.raises(WorkspaceUnavailableError, match="machine no longer exists"):
        await backend.working_dir()
    assert backend.ref == WorkspaceRef(provider="smol", id="deleted")


async def test_output_limit_stops_stream(
    monkeypatch: pytest.MonkeyPatch,
    machine_sdk: tuple[FakeMachine, list[MachineConfig]],
) -> None:
    machine, _ = machine_sdk
    monkeypatch.setattr(_backend, "_MAX_OUTPUT_BYTES", 4)
    closed = False

    async def large_output(
        command: list[str], opts: ExecOptions | None = None
    ) -> AsyncIterator[dict[str, object]]:
        nonlocal closed
        del command, opts
        try:
            yield {"kind": "stdout", "data": "out"}
            yield {"kind": "stderr", "data": "too much"}
            yield {"kind": "exit", "exit_code": 0}
        finally:
            closed = True

    monkeypatch.setattr(machine, "exec_stream", large_output)
    with pytest.raises(WorkspaceOutputLimitError, match="10 MiB") as caught:
        await SmolSandboxBackend().run(["echo", "long"])
    assert caught.value.stdout == "out" and caught.value.stderr == ""
    assert closed


async def test_transport_timeout_does_not_claim_command_deadline(
    monkeypatch: pytest.MonkeyPatch,
    machine_sdk: tuple[FakeMachine, list[MachineConfig]],
) -> None:
    machine, _ = machine_sdk

    async def unreachable(
        command: list[str], opts: ExecOptions | None = None
    ) -> AsyncIterator[dict[str, object]]:
        del command, opts
        raise SmolError("TIMEOUT", "socket timed out")
        yield  # Keep this an async generator, as the SDK's exec_stream is.

    async def running() -> str:
        return "running"

    monkeypatch.setattr(machine, "exec_stream", unreachable)
    monkeypatch.setattr(machine, "state", running, raising=False)
    with pytest.raises(SmolError, match="socket timed out") as caught:
        await SmolSandboxBackend().run(["true"], timeout=30)
    assert caught.value.code == "TIMEOUT"


async def test_agent_uses_smol_workspace(
    machine_sdk: tuple[FakeMachine, list[MachineConfig]],
) -> None:
    from pydantic_ai import Agent
    from pydantic_ai.models.test import TestModel

    machine, configs = machine_sdk
    agent = Agent(TestModel(call_tools=["read_vm"]), capabilities=[SmolSandbox()])

    @agent.tool
    async def read_vm(ctx: RunContext[object]) -> str:
        return (await ctx.workspace.run(["echo", "ok"])).stdout

    result = await agent.run("Read from the VM.")
    assert "out" in result.output
    assert result.workspace.ref == WorkspaceRef(provider="smol", id=machine.id)
    assert len(configs) == 1


async def test_cloud_creation_uses_bounded_lifetime(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    machine = FakeMachine()
    created: list[MachineConfig] = []

    async def create(config: MachineConfig, conn: ConnectOptions) -> FakeMachine:
        assert conn.target == "cloud" and conn.api_key == "secret"
        created.append(config)
        return machine

    monkeypatch.setattr(AsyncMachine, "create", create)
    backend = SmolSandboxBackend(target="cloud", api_key="secret", ttl_seconds=7200)
    await backend.get_machine()
    assert len(created) == 1 and created[0].ttl_seconds == 7200
    assert created[0].persistent
    assert "secret" not in repr(backend)
    assert "secret" not in repr(SmolSandbox[None](target="cloud", api_key="secret"))
