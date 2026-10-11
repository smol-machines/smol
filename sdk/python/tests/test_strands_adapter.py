"""Strands' real sandbox types over fake Smol machines, including cancellation."""

from __future__ import annotations

import asyncio
import math
import threading
from contextlib import aclosing
from unittest.mock import Mock, patch

import pytest
from smol import ConnectOptions
from smol.strands import SmolSandbox
from strands.sandbox import ExecutionResult, StreamChunk
from strands.sandbox.errors import SandboxTimeoutError


class Events:
    def __init__(self, values):
        self.values = iter(values)
        self.closed = False

    def __iter__(self):
        return self

    def __next__(self):
        return next(self.values)

    def close(self):
        self.closed = True


def test_lazy_vm_stream_files_and_owned_cleanup():
    async def exercise():
        machine = Mock()
        machine.id = "vm-owned"
        events = Events(
            [
                {"kind": "stdout", "data": "hello "},
                {"kind": "stderr", "data": "warning"},
                {"kind": "stdout", "data": "world"},
                {"kind": "exit", "exit_code": 7},
            ]
        )
        machine.exec_stream.return_value = events
        machine.read_file.return_value = b"\x00\xff"
        with patch("smol.strands.Machine.create", return_value=machine) as create:
            async with SmolSandbox() as sandbox:
                assert sandbox.machine_id is None
                assert [tool.tool_name for tool in sandbox.get_tools()] == [
                    "sandbox_file_editor",
                    "sandbox_shell",
                ]
                collected = [
                    event
                    async for event in sandbox.execute_streaming(
                        "echo hi", cwd="/source", env={"FOO": "bar"}
                    )
                ]
                assert [(e.stream_type, e.data) for e in collected[:-1]] == [
                    ("stdout", "hello "),
                    ("stderr", "warning"),
                    ("stdout", "world"),
                ]
                assert collected[-1] == ExecutionResult(
                    exit_code=7, stdout="hello world", stderr="warning"
                )
                assert events.closed
                assert sandbox.machine_id == "vm-owned"
                config, connection = create.call_args.args
                assert (
                    config.resources.network is False and connection.target == "local"
                )
                cmd, options = machine.exec_stream.call_args.args
                assert cmd == ["sh", "-c", "echo hi"]
                assert (options.workdir, options.env) == ("/source", {"FOO": "bar"})
                assert await sandbox.read_file("/workspace/raw.bin") == b"\x00\xff"
                await sandbox.write_file("/workspace/out.bin", b"\xff")
                machine.write_file.assert_called_once_with(
                    "/workspace/out.bin", b"\xff"
                )
            machine.delete.assert_called_once_with()
            with pytest.raises(RuntimeError, match="closed"):
                await sandbox.execute("echo again")

    asyncio.run(exercise())


def test_borrowed_vm_preserves_ownership_and_timeout_data():
    async def exercise():
        machine = Mock()
        machine.id = "external"
        events = Events(
            [
                {"kind": "stdout", "data": "before"},
                {"kind": "error", "message": "command timed out"},
            ]
        )
        machine.exec_stream.return_value = events
        async with SmolSandbox(machine=machine) as sandbox:
            assert sandbox.machine_id == "external"
            with pytest.raises(SandboxTimeoutError) as error:
                await sandbox.execute("sleep 10", timeout=0.1)
            assert error.value.stdout == "before"
            assert events.closed
        machine.delete.assert_not_called()

    asyncio.run(exercise())


def test_stream_close_kills_command_before_exit():
    async def exercise():
        machine = Mock()
        stream = Events(
            [{"kind": "stdout", "data": "first"}, {"kind": "exit", "exit_code": 0}]
        )
        machine.exec_stream.return_value = stream
        async with SmolSandbox(machine=machine) as sandbox:
            async with aclosing(sandbox.execute_streaming("long-running")) as chunks:
                first = await anext(chunks)
                assert first == StreamChunk(data="first", stream_type="stdout")
            assert stream.closed

    asyncio.run(exercise())


def test_cancellation_closes_inflight_stream():
    async def exercise():
        started = threading.Event()
        stopped = threading.Event()

        class BlockingStream:
            def __next__(self):
                started.set()
                stopped.wait(3)
                raise StopIteration

            def close(self):
                stopped.set()

        machine = Mock()
        machine.exec_stream.return_value = BlockingStream()
        async with SmolSandbox(machine=machine) as sandbox:
            task = asyncio.create_task(sandbox.execute("sleep 10"))
            assert await asyncio.to_thread(started.wait, 3)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert stopped.is_set(), "cancelling an agent must stop its VM command"

    asyncio.run(exercise())


def test_cloud_default_expires_if_caller_exits_and_invalid_env_does_not_boot_vm():
    async def exercise():
        machine = Mock()
        machine.exec_stream.return_value = Events([{"kind": "exit", "exit_code": 0}])
        with patch("smol.strands.Machine.create", return_value=machine) as create:
            async with SmolSandbox(conn=ConnectOptions(target="cloud")) as sandbox:
                with pytest.raises(
                    ValueError, match="Invalid environment variable name"
                ):
                    await sandbox.execute("true", env={"FOO=x": "wrong"})
                for timeout in (0, math.inf, math.nan):
                    with pytest.raises(ValueError, match="positive and finite"):
                        await sandbox.execute("true", timeout=timeout)
                create.assert_not_called()
                assert (await sandbox.execute("true")).exit_code == 0
                assert create.call_args.args[0].ttl_seconds == 3600

    asyncio.run(exercise())


def test_agent_registers_vm_shell_and_file_editor():
    from strands import Agent

    machine = Mock()
    machine.exec_stream.side_effect = lambda *args: Events(
        [{"kind": "stdout", "data": "42"}, {"kind": "exit", "exit_code": 0}]
    )
    sandbox = SmolSandbox(machine=machine)
    try:
        agent = Agent(model="nonsense", sandbox=sandbox)
        assert agent.sandbox is sandbox
        assert {"sandbox_file_editor", "sandbox_shell"}.issubset(
            agent.tool_registry.registry
        )
        shell = agent.tool_registry.registry["sandbox_shell"]
        result = asyncio.run(shell(command="printf 42", tool_context=None))
        assert result == {"output": "42", "error": "", "exit_code": 0}
        editor = agent.tool_registry.registry["sandbox_file_editor"]
        created = asyncio.run(
            editor(
                command="create",
                path="/workspace/answer.txt",
                file_text="42\n",
                tool_context=None,
            )
        )
        assert "File created successfully" in created
        machine.write_file.assert_called_once_with("/workspace/answer.txt", b"42\n")
    finally:
        sandbox.close()


def test_host_deadline_stops_a_blocked_command_and_preserves_partial_output():
    async def exercise():
        blocked = threading.Event()
        stopped = threading.Event()

        class SlowStream:
            first = True

            def __next__(self):
                if self.first:
                    self.first = False
                    return {"kind": "stdout", "data": "before"}
                blocked.set()
                stopped.wait(3)
                raise StopIteration

            def close(self):
                stopped.set()

        machine = Mock()
        machine.exec_stream.return_value = SlowStream()
        async with SmolSandbox(machine=machine) as sandbox:
            with pytest.raises(SandboxTimeoutError) as error:
                await sandbox.execute("sleep 10", timeout=0.05)
            assert error.value.stdout == "before"
            assert blocked.is_set() and stopped.is_set()
            assert machine.exec_stream.call_args.args[1].timeout == 2

    asyncio.run(exercise())


def test_host_deadline_covers_slow_stream_open_and_closes_late_command():
    async def exercise():
        opening = threading.Event()
        release = threading.Event()
        stream = Events([{"kind": "exit", "exit_code": 0}])

        def delayed_open(*args):
            opening.set()
            assert release.wait(3)
            return stream

        machine = Mock()
        machine.exec_stream.side_effect = delayed_open
        async with SmolSandbox(machine=machine) as sandbox:
            with pytest.raises(SandboxTimeoutError) as error:
                await sandbox.execute("sleep 10", timeout=0.05)
            assert (error.value.stdout, error.value.stderr) == ("", "")
            assert opening.is_set()
            release.set()
            for _ in range(100):
                if stream.closed:
                    break
                await asyncio.sleep(0.01)
            assert stream.closed, "a late stream must not leave a command running"

    asyncio.run(exercise())


def test_cancelling_while_stream_opens_stops_late_command():
    async def exercise():
        opening = threading.Event()
        release = threading.Event()
        closed = threading.Event()
        stream = Events([{"kind": "exit", "exit_code": 0}])

        def delayed_open(*args):
            opening.set()
            assert release.wait(3)
            return stream

        def close():
            closed.set()

        stream.close = close
        machine = Mock()
        machine.exec_stream.side_effect = delayed_open
        async with SmolSandbox(machine=machine) as sandbox:
            task = asyncio.create_task(sandbox.execute("sleep 5"))
            assert await asyncio.to_thread(opening.wait, 3)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            release.set()
            assert await asyncio.to_thread(closed.wait, 3)

    asyncio.run(exercise())


def test_failed_vm_deletion_can_be_retried():
    from smol.errors import SmolError

    machine = Mock()
    machine.id = "owned"
    machine.exec_stream.return_value = Events([{"kind": "exit", "exit_code": 0}])
    machine.delete.side_effect = [SmolError("IO", "temporary error"), None]
    with patch("smol.strands.Machine.create", return_value=machine):
        sandbox = SmolSandbox()
        assert asyncio.run(sandbox.execute("true")).exit_code == 0
        with pytest.raises(SmolError, match="temporary error"):
            sandbox.close()
        sandbox.close()
    assert machine.delete.call_count == 2


def test_exit_code_124_is_preserved_when_the_command_exits_before_its_deadline():
    async def exercise():
        machine = Mock()
        machine.exec_stream.return_value = Events([{"kind": "exit", "exit_code": 124}])
        async with SmolSandbox(machine=machine) as sandbox:
            result = await sandbox.execute("exit 124", timeout=10)
            assert result.exit_code == 124

    asyncio.run(exercise())
