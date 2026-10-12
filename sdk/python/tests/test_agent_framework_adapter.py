"""Agent Framework shell contract for a disposable local or Cloud Smol VM."""

from __future__ import annotations

import asyncio
from typing import ClassVar

import pytest

pytest.importorskip("agent_framework_tools")

from agent_framework.tools import ShellCommandError, ShellExecutionError, ShellPolicy

from smol.agent_framework import SmolShellTool
from smol.errors import SmolError
from smol.types import ConnectOptions


class FakeMachine:
    created: ClassVar[list[tuple[object, object]]] = []
    instances: ClassVar[list[FakeMachine]] = []

    def __init__(self, *, exit_code: int = 0, stdout: bytes = b"hi\n", stderr: bytes = b"") -> None:
        self.exit_code = exit_code
        self.stdout = stdout
        self.stderr = stderr
        self.commands: list[tuple[list[str], object]] = []
        self.deleted = False
        self.wait = asyncio.Event()
        self.wait.set()
        FakeMachine.instances.append(self)

    @classmethod
    async def create(cls, config: object, conn: object) -> FakeMachine:
        cls.created.append((config, conn))
        return cls()

    async def exec_stream(self, command: list[str], opts: object):
        self.commands.append((command, opts))
        await self.wait.wait()
        if self.stdout:
            yield {"kind": "stdout", "data": self.stdout.decode("utf-8", "replace")}
        if self.stderr:
            yield {"kind": "stderr", "data": self.stderr.decode("utf-8", "replace")}
        yield {"kind": "exit", "exit_code": self.exit_code}

    async def delete(self) -> None:
        self.deleted = True


@pytest.fixture(autouse=True)
def reset_fake(monkeypatch: pytest.MonkeyPatch) -> None:
    FakeMachine.created.clear()
    FakeMachine.instances.clear()
    monkeypatch.setattr("smol.agent_framework.AsyncMachine", FakeMachine)


def test_owned_vm_uses_guest_shell_and_deletes_on_exit() -> None:
    async def scenario() -> None:
        async with SmolShellTool() as shell:
            result = await shell.run("echo hi")
            machine = FakeMachine.instances[0]
            assert result.stdout == "hi\n" and result.exit_code == 0
            assert machine.commands[0][0] == ["/bin/sh", "-c", 'exec "$@" </dev/null', "sh", "/bin/sh", "-c", "echo hi"]
            assert machine.commands[0][1].workdir == "/workspace"
            assert machine.commands[0][1].timeout == 30
            assert FakeMachine.created[0][0].resources.network is False
            assert not machine.deleted
        assert machine.deleted
        await shell.close()
    asyncio.run(scenario())


def test_cloud_vm_has_ttl_and_nonzero_exit_is_a_result() -> None:
    async def scenario() -> None:
        conn = ConnectOptions(target="cloud")
        shell = SmolShellTool(conn=conn)
        async with shell:
            FakeMachine.instances[0].exit_code = 7
            result = await shell.run("false", timeout=2.5)
            assert result.exit_code == 7
            assert FakeMachine.created[0][0].ttl_seconds == 3600
            assert FakeMachine.created[0][1] is conn
            assert FakeMachine.instances[0].commands[0][1].timeout == 2.5
    asyncio.run(scenario())


def test_existing_machine_is_never_deleted_and_policy_denies_before_execution() -> None:
    async def scenario() -> None:
        machine = FakeMachine()
        async with SmolShellTool(machine=machine, policy=ShellPolicy(denylist=["^deny"])) as shell:
            with pytest.raises(ShellCommandError):
                await shell.run("deny this")
            result = await shell.run("allowed")
            assert result.exit_code == 0
        assert machine.commands[0][0][-1] == "allowed"
        assert not machine.deleted
        assert not FakeMachine.created
    asyncio.run(scenario())


def test_tool_exposes_approved_shell_and_structured_results() -> None:
    async def scenario() -> None:
        machine = FakeMachine(exit_code=17, stdout=b"out", stderr=b"error")
        shell = SmolShellTool(machine=machine)
        function = shell.as_function()
        assert function.kind == "shell"
        assert function.approval_mode == "always_require"
        outcome = await function.invoke(arguments={"command": "fail"})
        assert outcome[0].additional_properties["exit_code"] == 17
        assert outcome[0].additional_properties["stderr"] == "error"
        assert "exit_code: 17" in outcome[0].text
    asyncio.run(scenario())


def test_exec_error_uses_framework_error_type(monkeypatch: pytest.MonkeyPatch) -> None:
    async def scenario() -> None:
        machine = FakeMachine()

        async def broken_exec(*_args: object):
            raise SmolError("NOT_FOUND", "machine stopped")
            yield  # Keep this as an async iterator for the streaming API.

        monkeypatch.setattr(machine, "exec_stream", broken_exec)
        with pytest.raises(ShellExecutionError, match="machine stopped") as error:
            await SmolShellTool(machine=machine).run("echo hi")
        assert isinstance(error.value.__cause__, SmolError)
        assert error.value.__cause__.code == "NOT_FOUND"
    asyncio.run(scenario())


def test_shell_environment_provider_probes_the_guest() -> None:
    from agent_framework.tools import (
        ShellEnvironmentProvider,
        ShellEnvironmentProviderOptions,
        ShellFamily,
    )

    async def scenario() -> None:
        machine = FakeMachine(stdout=b"VERSION=unknown\nCWD=/workspace\n")
        shell = SmolShellTool(machine=machine)
        provider = ShellEnvironmentProvider(
            shell,
            ShellEnvironmentProviderOptions(probe_tools=(), override_family=ShellFamily.POSIX),
        )
        snapshot = await provider.refresh()
        assert snapshot.family is ShellFamily.POSIX
        assert snapshot.working_directory == "/workspace"
        assert machine.commands[0][0][0] == "/bin/sh"
    asyncio.run(scenario())


def test_output_cap_preserves_both_streams() -> None:
    async def scenario() -> None:
        machine = FakeMachine(stdout=b"a" * 100, stderr=b"failure!" * 10)
        result = await SmolShellTool(machine=machine, max_output_bytes=20).run("print")
        assert result.truncated
        assert len(result.stdout.encode()) + len(result.stderr.encode()) <= 20
        assert result.stderr.endswith("re!")
    asyncio.run(scenario())


def test_cancellation_waits_for_inflight_guest_exec() -> None:
    async def scenario() -> None:
        machine = FakeMachine()
        machine.wait.clear()
        shell = SmolShellTool(machine=machine)
        task = asyncio.create_task(shell.run("long command"))
        for _ in range(20):
            if machine.commands:
                break
            await asyncio.sleep(0)
        assert machine.commands
        task.cancel()
        await asyncio.sleep(0)
        assert not task.done(), "guest command must finish before cancellation returns"
        task.cancel()  # Repeated cancellation must not race the guest exec.
        machine.wait.set()
        with pytest.raises(asyncio.CancelledError):
            await task
        await shell.close()
        assert not machine.deleted
    asyncio.run(scenario())


def test_cancelled_create_cleans_up_vm(monkeypatch: pytest.MonkeyPatch) -> None:
    async def scenario() -> None:
        creating = asyncio.Event()
        allow_create = asyncio.Event()
        original_create = FakeMachine.create

        async def delayed_create(cls: type[FakeMachine], config: object, conn: object) -> FakeMachine:
            creating.set()
            await allow_create.wait()
            return await original_create(config, conn)

        monkeypatch.setattr(FakeMachine, "create", classmethod(delayed_create))
        shell = SmolShellTool(conn=ConnectOptions(target="cloud"))
        start = asyncio.create_task(shell.start())
        await creating.wait()
        start.cancel()
        await asyncio.sleep(0)
        assert not start.done()
        start.cancel()  # Repeated cancellation cannot orphan the in-flight create.
        allow_create.set()
        with pytest.raises(asyncio.CancelledError):
            await start
        assert FakeMachine.instances[0].deleted
        assert shell.machine is None
    asyncio.run(scenario())


def test_cancelled_close_finishes_delete(monkeypatch: pytest.MonkeyPatch) -> None:
    async def scenario() -> None:
        shell = SmolShellTool()
        await shell.start()
        machine = FakeMachine.instances[0]
        deleting = asyncio.Event()
        allow_delete = asyncio.Event()

        async def delayed_delete() -> None:
            deleting.set()
            await allow_delete.wait()
            machine.deleted = True

        monkeypatch.setattr(machine, "delete", delayed_delete)
        close = asyncio.create_task(shell.close())
        await deleting.wait()
        close.cancel()
        await asyncio.sleep(0)
        assert not close.done()
        allow_delete.set()
        with pytest.raises(asyncio.CancelledError):
            await close
        assert machine.deleted
        assert shell.machine is None
    asyncio.run(scenario())


def test_failed_delete_preserves_ownership_for_retry(monkeypatch: pytest.MonkeyPatch) -> None:
    async def scenario() -> None:
        shell = SmolShellTool()
        await shell.start()
        machine = FakeMachine.instances[0]
        original_delete = machine.delete
        attempts = 0

        async def flaky_delete() -> None:
            nonlocal attempts
            attempts += 1
            if attempts == 1:
                raise OSError("delete failed")
            await original_delete()

        monkeypatch.setattr(machine, "delete", flaky_delete)
        with pytest.raises(OSError, match="delete failed"):
            await shell.close()
        assert shell.machine is machine
        await shell.close()
        assert machine.deleted and shell.machine is None
    asyncio.run(scenario())


def test_invalid_settings_fail_before_vm_creation() -> None:
    with pytest.raises(ValueError):
        SmolShellTool(timeout=0)
    with pytest.raises(ValueError):
        SmolShellTool(workdir="relative")
    with pytest.raises(ValueError):
        SmolShellTool(machine=FakeMachine(), conn=ConnectOptions(target="cloud"))


def test_streamed_output_keeps_bounded_head_tail() -> None:
    from smol.agent_framework import _CapturedStream

    output = _CapturedStream(20)
    for _ in range(128):
        output.append("begin" + "x" * 1000 + "end")
    assert output.total > 100_000
    assert len(output.head) + len(output.tail) <= 20
    assert output.view(20).startswith("begin")
    assert output.view(20).endswith("end")


def test_guest_command_timeout_returns_typed_result() -> None:
    async def scenario() -> None:
        machine = FakeMachine()
        machine.wait.clear()
        result = await SmolShellTool(machine=machine).run("sleep 10", timeout=0.01)
        assert result.exit_code == 124 and result.timed_out

    asyncio.run(scenario())


def test_guest_stream_without_exit_fails() -> None:
    async def scenario() -> None:
        machine = FakeMachine()

        async def missing_exit(*_args: object):
            yield {"kind": "stdout", "data": "incomplete"}

        machine.exec_stream = missing_exit  # type: ignore[method-assign]
        with pytest.raises(ShellExecutionError, match="without an exit status"):
            await SmolShellTool(machine=machine).run("echo incomplete")

    asyncio.run(scenario())
