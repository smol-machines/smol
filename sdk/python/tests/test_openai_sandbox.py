"""OpenAI SandboxAgent provider contract without a model API key or KVM."""

import asyncio
import io
import tarfile
from pathlib import Path
from typing import ClassVar

import pytest

pytest.importorskip("agents.sandbox")
from agents.sandbox.manifest import Manifest
from agents.sandbox.snapshot import NoopSnapshotSpec
from smol import openai_sandbox as adapter
from smol.errors import SmolError
from smol.types import ExecResult


class FakeMachine:
    created: ClassVar[list] = []
    instances: ClassVar[dict] = {}

    def __init__(self, machine_id):
        self.id = machine_id
        self.files = {}
        self.deleted = False
        self.commands = []
        self.instances[machine_id] = self

    @classmethod
    async def create(cls, config, conn):
        machine = cls(f"mock-{len(cls.created)}")
        cls.created.append((config, conn))
        return machine

    @classmethod
    async def connect(cls, machine_id, conn):
        machine = cls.instances.get(machine_id)
        if machine is None or machine.deleted:
            raise SmolError("NOT_FOUND", "VM not found")
        return machine

    async def state(self):
        return "started"

    async def wait_until_ready(self):
        pass

    async def exec(self, command, opts=None):
        self.commands.append(command)
        return ExecResult(exit_code=0, stdout="", stderr="")

    async def read_file(self, path):
        return self.files[path]

    async def write_file(self, path, data):
        self.files[path] = data

    async def ready(self):
        return True

    async def delete(self):
        self.deleted = True


@pytest.fixture(autouse=True)
def fake_machine(monkeypatch):
    FakeMachine.created.clear()
    FakeMachine.instances.clear()
    monkeypatch.setattr(adapter, "AsyncMachine", FakeMachine)


def test_machine_lifecycle_state_round_trip_and_network_policy():
    async def scenario():
        client = adapter.SmolSandboxClient(target="cloud")
        options = adapter.SmolSandboxClientOptions(
            image="alpine:3.20", ttl_seconds=300, branchable=True
        )
        session = await client.create(options=options)
        try:
            await session.start()
            config, conn = FakeMachine.created[0]
            assert config.image == "alpine:3.20"
            assert config.branchable is True
            assert config.resources.network is False
            assert conn.target == "cloud"
            await session.write(Path("/workspace/answer"), io.BytesIO(b"42"))
            payload = client.serialize_session_state(session.state)
            resumed = await client.resume(client.deserialize_session_state(payload))
            assert (await resumed.read(Path("/workspace/answer"))).read() == b"42"
        finally:
            await client.delete(session)

    asyncio.run(scenario())


def test_missing_machine_without_snapshot_never_creates_an_empty_replacement():
    async def scenario():
        client = adapter.SmolSandboxClient()
        session = await client.create(snapshot=NoopSnapshotSpec())
        payload = client.serialize_session_state(session.state)
        await client.delete(session)
        resumed = await client.resume(client.deserialize_session_state(payload))
        with pytest.raises(RuntimeError, match="snapshot is unavailable"):
            await resumed.start()
        assert len(FakeMachine.created) == 1

    asyncio.run(scenario())


def test_resuming_started_session_without_snapshot_does_not_replace_machine():
    async def scenario():
        client = adapter.SmolSandboxClient()
        session = await client.create(snapshot=NoopSnapshotSpec())
        await session.start()
        payload = client.serialize_session_state(session.state)
        await client.delete(session)
        with pytest.raises(RuntimeError, match="snapshot is unavailable"):
            await client.resume(client.deserialize_session_state(payload))
        assert len(FakeMachine.created) == 1

    asyncio.run(scenario())


def test_archive_validation_precedes_workspace_removal():
    async def scenario():
        client = adapter.SmolSandboxClient()
        session = await client.create()
        await session.start()
        machine = FakeMachine.instances[session.state.machine_id]
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            entry = tarfile.TarInfo("../../host-file")
            entry.size = 1
            tar.addfile(entry, io.BytesIO(b"x"))
        with pytest.raises(Exception, match=r"unsafe|traversal|escape|invalid|\.\."):
            await session.hydrate_workspace(io.BytesIO(archive.getvalue()))
        assert not any(cmd[0] == "sh" for cmd in machine.commands)
        await client.delete(session)

    asyncio.run(scenario())


def test_manifest_mounts_and_wrong_target_rejected_before_creation():
    async def scenario():
        client = adapter.SmolSandboxClient()
        with pytest.raises(ValueError, match="/workspace"):
            await client.create(manifest=Manifest(root="/other"))
        cloud = adapter.SmolSandboxClient(target="cloud")
        with pytest.raises(ValueError, match="require an image"):
            await cloud.create()
        assert FakeMachine.created == []

    asyncio.run(scenario())


def test_network_access_requires_trusted_client_policy():
    async def scenario():
        with pytest.raises(ValueError, match="allow_network"):
            await adapter.SmolSandboxClient().create(
                options=adapter.SmolSandboxClientOptions(network=True)
            )
        assert FakeMachine.created == []

    asyncio.run(scenario())


def test_network_access_remains_authorized_when_reattaching():
    async def scenario():
        client = adapter.SmolSandboxClient(allow_network=True)
        session = await client.create(
            options=adapter.SmolSandboxClientOptions(allow_hosts=("github.com",))
        )
        try:
            await session.start()
            serialized = client.serialize_session_state(session.state)
            untrusted = adapter.SmolSandboxClient()
            with pytest.raises(ValueError, match="allow_network=True"):
                await untrusted.resume(untrusted.deserialize_session_state(serialized))
        finally:
            await client.delete(session)

    asyncio.run(scenario())


def test_sandbox_agent_shell_runs_in_machine_and_runner_deletes_it():
    from agents import RunConfig, Runner
    from agents.sandbox import SandboxAgent, SandboxRunConfig
    from agents.sandbox.capabilities import Shell
    from agents.testing import ScriptedModel
    from openai.types.responses import (
        ResponseFunctionToolCall,
        ResponseOutputMessage,
        ResponseOutputText,
    )

    async def scenario():
        model = ScriptedModel()
        model.enqueue(
            [
                ResponseFunctionToolCall(
                    id="tool-1",
                    call_id="exec-1",
                    type="function_call",
                    name="exec_command",
                    arguments='{"cmd":"printf 42","login":false}',
                )
            ]
        )
        model.enqueue(
            [
                ResponseOutputMessage(
                    id="answer-1",
                    type="message",
                    role="assistant",
                    content=[
                        ResponseOutputText(
                            text="completed",
                            type="output_text",
                            annotations=[],
                            logprobs=[],
                        )
                    ],
                    status="completed",
                )
            ]
        )
        agent = SandboxAgent(
            name="Smol agent",
            model=model,
            instructions="Use the shell tool.",
            capabilities=[Shell()],
        )
        result = await Runner.run(
            agent,
            "Print 42 in a sandbox",
            run_config=RunConfig(
                sandbox=SandboxRunConfig(
                    client=adapter.SmolSandboxClient(), snapshot=NoopSnapshotSpec()
                )
            ),
        )
        machine = FakeMachine.instances["mock-0"]
        assert result.final_output == "completed"
        assert any(command[:2] == ["sh", "-c"] for command in machine.commands)
        assert machine.deleted

    asyncio.run(scenario())


def test_local_missing_file_preserves_agents_not_found_error():
    from agents.sandbox.errors import WorkspaceReadNotFoundError

    async def scenario():
        client = adapter.SmolSandboxClient()
        session = await client.create()
        machine = FakeMachine.instances[session.state.machine_id]

        async def missing(_path):
            raise SmolError(
                "SMOLVM_ERROR",
                "failed to open file: No such file or directory (os error 2)",
            )

        machine.read_file = missing
        with pytest.raises(WorkspaceReadNotFoundError):
            await session.read(Path("/workspace/missing.txt"))
        await client.delete(session)

    asyncio.run(scenario())


def test_bounded_read_does_not_silence_guest_read_failures():
    from agents.sandbox.errors import (
        WorkspaceArchiveReadError,
        WorkspaceReadNotFoundError,
    )

    async def scenario():
        client = adapter.SmolSandboxClient()
        session = await client.create()
        machine = FakeMachine.instances[session.state.machine_id]

        async def unreadable(_command, _opts=None):
            return ExecResult(exit_code=43, stdout="", stderr="Permission denied")

        machine.exec = unreadable
        with pytest.raises(WorkspaceArchiveReadError):
            await session.read_bounded(Path("/workspace/unreadable"), max_bytes=128)

        async def missing(_command, _opts=None):
            return ExecResult(exit_code=42, stdout="", stderr="")

        machine.exec = missing
        with pytest.raises(WorkspaceReadNotFoundError):
            await session.read_bounded(Path("/workspace/missing"), max_bytes=128)
        await client.delete(session)
        await client.delete(session)

    asyncio.run(scenario())


def test_archive_cannot_write_through_symlink_outside_workspace():
    from agents.sandbox.util.tar_utils import UnsafeTarMemberError

    async def scenario():
        client = adapter.SmolSandboxClient()
        session = await client.create()
        machine = FakeMachine.instances[session.state.machine_id]
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            link = tarfile.TarInfo("escape")
            link.type = tarfile.SYMTYPE
            link.linkname = "/etc"
            tar.addfile(link)
            file = tarfile.TarInfo("escape/passwd")
            file.size = 1
            tar.addfile(file, io.BytesIO(b"x"))
        with pytest.raises(UnsafeTarMemberError):
            await session.hydrate_workspace(io.BytesIO(archive.getvalue()))
        assert machine.commands == []
        await client.delete(session)

    asyncio.run(scenario())


def test_scoped_egress_preserves_allowlist_without_enabling_open_network():
    async def scenario():
        client = adapter.SmolSandboxClient(allow_network=True)
        session = await client.create(
            options=adapter.SmolSandboxClientOptions(allow_hosts=("github.com",))
        )
        try:
            config, _conn = FakeMachine.created[0]
            assert config.resources.allow_hosts == ["github.com"]
            assert config.resources.network is None
        finally:
            await client.delete(session)

    asyncio.run(scenario())
    with pytest.raises(
        ValueError, match="choose unrestricted network or scoped egress"
    ):
        adapter.SmolSandboxClientOptions(network=True, allow_hosts=("github.com",))
