"""Workspace lifecycle and command boundaries against Pydantic AI's real protocol."""

import asyncio

import pytest

pytest.importorskip('pydantic_ai')
from pydantic_ai.workspaces import (  # noqa: E402
    Workspace,
    WorkspaceOutputLimitError,
    WorkspaceTimeoutError,
    WorkspaceUnavailableError,
)

from smol import AsyncMachine, ExecResult, SmolError  # noqa: E402
from smol import pydantic_ai as adapter  # noqa: E402


def test_concurrent_acquisition_survives_caller_cancellation(monkeypatch):
    async def scenario():
        created = []
        setup_started = asyncio.Event()
        finish_setup = asyncio.Event()

        class Machine:
            id = 'test-vm'

            async def exec(self, args):
                setup_started.set()
                await finish_setup.wait()
                return ExecResult(0, '', '')

        async def create(config):
            created.append(config)
            return Machine()

        monkeypatch.setattr(AsyncMachine, 'create', create)
        backend = adapter.SmolWorkspaceBackend(image='/tmp/offline-image.tar')
        first = asyncio.create_task(backend.working_dir())
        await setup_started.wait()
        first.cancel()
        with pytest.raises(asyncio.CancelledError):
            await first
        second = asyncio.create_task(backend.working_dir())
        await asyncio.sleep(0)
        assert not second.done()
        finish_setup.set()
        assert await second == '/workspace'
        assert backend.ref.provider == 'smol-local'
        assert backend.ref.id == 'test-vm'
        assert len(created) == 1
        assert created[0].persistent is True
        assert created[0].image == '/tmp/offline-image.tar'
        assert created[0].resources is None

    asyncio.run(scenario())


def test_stream_output_cap_kills_command_and_preserves_partial_output(monkeypatch):
    async def scenario():
        closed = []

        class Machine:
            async def exec_stream(self, args, opts):
                try:
                    assert args == ['sh', '-c', 'generate lots']
                    assert opts.workdir == '/workspace'
                    yield {'kind': 'stdout', 'data': 'abc'}
                    yield {'kind': 'stderr', 'data': 'def'}
                    yield {'kind': 'exit', 'exit_code': 0}
                finally:
                    closed.append(True)

        monkeypatch.setattr(adapter, '_MAX_OUTPUT_BYTES', 4)
        backend = adapter.SmolWorkspaceBackend()
        backend._machine = Machine()
        with pytest.raises(WorkspaceOutputLimitError) as caught:
            await Workspace(backend).run('generate lots', shell=True)
        assert caught.value.stdout == 'abc'
        assert caught.value.stderr == 'd'
        assert closed

    asyncio.run(scenario())


def test_timeout_reports_partial_output_and_closes_stream():
    async def scenario():
        closed = []

        class Machine:
            async def exec_stream(self, args, opts):
                try:
                    yield {'kind': 'stdout', 'data': 'partial'}
                    await asyncio.sleep(10)
                finally:
                    closed.append(True)

        backend = adapter.SmolWorkspaceBackend()
        backend._machine = Machine()
        with pytest.raises(WorkspaceTimeoutError) as caught:
            await Workspace(backend).run(['sleep', '10'], timeout=0.01)
        assert caught.value.stdout == 'partial'
        assert closed

    asyncio.run(scenario())


def test_missing_reference_does_not_create_another_vm(monkeypatch):
    async def scenario():
        async def connect(ref):
            raise SmolError('NOT_FOUND', 'VM does not exist')

        async def create(config):
            raise AssertionError('must never create a replacement')

        monkeypatch.setattr(AsyncMachine, 'connect', connect)
        monkeypatch.setattr(AsyncMachine, 'create', create)
        ref = adapter.WorkspaceRef(provider='smol-local', id='deleted-vm')
        with pytest.raises(WorkspaceUnavailableError):
            await adapter.SmolWorkspaceBackend(ref).working_dir()

    asyncio.run(scenario())


def test_completed_command_flushes_before_reporting_success_or_nonzero():
    async def scenario():
        flushed = []

        class Machine:
            async def exec_stream(self, args, opts):
                yield {'kind': 'stdout', 'data': 'result'}
                yield {'kind': 'exit', 'exit_code': 124}

            async def exec(self, args, opts):
                flushed.append(args)
                return ExecResult(0, '', '')

        backend = adapter.SmolWorkspaceBackend()
        backend._machine = Machine()
        result = await Workspace(backend).run('exit 124', shell=True, timeout=1)
        assert result.exit_code == 124  # 124 from the program is not a timeout.
        assert result.stdout == 'result'
        assert flushed == [['sync']]

    asyncio.run(scenario())
