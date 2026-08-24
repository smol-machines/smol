"""Contract tests for the external NeMo Gym sandbox provider."""

from __future__ import annotations

import asyncio
from importlib.metadata import EntryPoint
from pathlib import Path
from typing import ClassVar

import pytest
import smol.nemo_gym as adapter
from nemo_gym.sandbox import AsyncSandbox
from nemo_gym.sandbox.providers.base import (
    ConnectableProvider,
    SandboxSpec,
    SandboxStatus,
)
from smol.types import ExecResult, PortEndpoint


class FakeMachine:
    created: ClassVar[list[tuple[object, object]]] = []
    connected: ClassVar[list[tuple[str, object]]] = []
    instances: ClassVar[list[FakeMachine]] = []

    def __init__(self, machine_id: str) -> None:
        self.id = machine_id
        self.name = machine_id
        self.commands: list[tuple[list[str], object]] = []
        self.files: dict[str, bytes] = {}
        self.deleted = False
        self.forks: list[str] = []
        self.fork_batches: list[list[str]] = []
        self.lifecycle_state = "running"
        FakeMachine.instances.append(self)

    @classmethod
    async def create(cls, config, conn):
        cls.created.append((config, conn))
        return cls(f"mach-{config.name}")

    @classmethod
    async def connect(cls, machine_id, conn):
        cls.connected.append((machine_id, conn))
        return cls(machine_id)

    async def fork(self, name):
        self.forks.append(name)
        return FakeMachine(f"fork-{name}")

    async def fork_batch(self, *, names):
        self.fork_batches.append(list(names))
        return [FakeMachine(f"fork-{name}") for name in names]

    async def assign(self, lease_id, *, ttl_secs=None):
        return FakeEpisode(FakeMachine(f"lease-{lease_id}"), lease_id, ttl_secs)

    async def exec(self, command, options=None):
        self.commands.append((list(command), options))
        return ExecResult(exit_code=0, stdout="smol-sandbox-ready", stderr="")

    async def write_file(self, path, data, mode=None):
        self.files[path] = bytes(data)
        self.files[f"{path}:mode"] = str(mode).encode()

    async def read_file(self, path):
        return self.files[path]

    async def state(self):
        return self.lifecycle_state

    async def wait_until_ready(self):
        return None

    def endpoint(self, port):
        return PortEndpoint(
            http_url=f"https://smol.test/{self.id}/{port}",
            ws_url=f"wss://smol.test/{self.id}/{port}",
            headers={"authorization": "Bearer test"},
        )

    async def delete(self):
        self.deleted = True


class FakeEpisode:
    def __init__(self, machine, lease_id, ttl_secs) -> None:
        self.machine = machine
        self.lease_id = lease_id
        self.ttl_secs = ttl_secs
        self.completed: list[str] = []

    async def complete(self, reason="done"):
        self.completed.append(reason)
        self.machine.deleted = True


@pytest.fixture(autouse=True)
def fake_sdk(monkeypatch):
    FakeMachine.created.clear()
    FakeMachine.connected.clear()
    FakeMachine.instances.clear()
    monkeypatch.setattr(adapter, "AsyncMachine", FakeMachine)


def test_published_nemo_gym_entry_point_loads_provider() -> None:
    entry_point = EntryPoint(
        name="smol",
        value="smol.nemo_gym:SmolProvider",
        group="nemo_gym.sandbox_providers",
    )
    assert entry_point.load() is adapter.SmolProvider


@pytest.mark.asyncio
async def test_cold_create_maps_full_spec_and_contract(tmp_path: Path) -> None:
    provider = adapter.SmolProvider(target="local")
    spec = SandboxSpec(
        image="python:3.12",
        ttl_s=60,
        ready_timeout_s=900,
        workdir="/workspace",
        env={"TASK": "one"},
        resources={"cpu": 1.25, "memory_mib": 2048, "disk_gib": 8},
        entrypoint=["sleep", "infinity"],
        ports=[8080],
        provider_options={
            "network_policy": {
                "defaultAction": "deny",
                "egress": [
                    {"action": "allow", "target": "https://api.example.com:443"},
                    {"action": "allow", "target": "10.0.0.0/8"},
                    {"action": "allow", "target": "192.0.2.5:8000"},
                ],
            }
        },
    )
    handle = await provider.create(spec)

    config, conn = FakeMachine.created[0]
    assert conn.target == "local"
    assert config.image == "python:3.12"
    assert config.command == ["sleep", "infinity"]
    assert config.ready_timeout_seconds == 900
    assert config.ttl_seconds == 60
    assert config.workdir == "/workspace"
    assert config.env == {"TASK": "one"}
    assert config.resources.cpus == 2
    assert config.resources.memory_mb == 2048
    assert config.resources.storage_gb == 8
    assert config.resources.allow_hosts == ["api.example.com"]
    assert config.resources.allow_cidrs == ["10.0.0.0/8", "192.0.2.5/32"]
    assert config.ports[0].guest == 8080 and config.ports[0].host > 0

    result = await provider.exec(
        handle,
        "printf $SECOND",
        cwd="/tmp",
        env={"SECOND": "two"},
        timeout_s=1.1,
    )
    assert result.return_code == 0
    command, options = handle.raw.machine.commands[-1]
    assert command == ["/bin/sh", "-lc", "printf $SECOND"]
    assert options.env == {"TASK": "one", "SECOND": "two"}
    assert options.workdir == "/tmp"
    assert options.timeout == 2

    source = tmp_path / "run.sh"
    source.write_bytes(b"#!/bin/sh\necho ok\n")
    source.chmod(0o750)
    await provider.upload_file(handle, source, "/workspace/bin/run.sh")
    handle.raw.machine.files["/workspace/out"] = b"result"
    target = tmp_path / "download" / "out"
    await provider.download_file(handle, "/workspace/out", target)
    assert target.read_bytes() == b"result"
    assert handle.raw.machine.files["/workspace/bin/run.sh"] == source.read_bytes()
    assert handle.raw.machine.files["/workspace/bin/run.sh:mode"] == b"488"

    assert await provider.status(handle) is SandboxStatus.RUNNING
    endpoint = await provider.endpoint(handle, 8080)
    assert endpoint.endpoint.endswith("/8080")
    assert endpoint.headers == {"authorization": "Bearer test"}
    await provider.close(handle)
    await provider.close(handle)
    assert handle.raw.machine.deleted is True
    assert await provider.status(handle) is SandboxStatus.STOPPED


@pytest.mark.asyncio
async def test_exact_checkpoint_forks_prepared_machine() -> None:
    provider = adapter.SmolProvider(
        target="cloud",
        checkpoints={
            "swe:ready": {
                "machine": "mach-golden",
                "entrypoint": ["/opt/ready"],
                "ports": [8000],
            }
        },
    )
    handle = await provider.create(
        SandboxSpec(image="swe:ready", entrypoint=["/opt/ready"], ports=[8000])
    )

    assert FakeMachine.created == []
    assert FakeMachine.connected[0][0] == "mach-golden"
    golden = next(
        machine for machine in FakeMachine.instances if machine.id == "mach-golden"
    )
    assert len(golden.fork_batches) == 1
    assert len(golden.fork_batches[0]) == 1
    assert handle.sandbox_id.startswith("fork-nemo-gym-")
    await provider.close(handle)
    assert handle.raw.machine.deleted is True
    assert golden.deleted is False


@pytest.mark.asyncio
async def test_cloud_checkpoint_ttl_uses_durable_episode_lease() -> None:
    provider = adapter.SmolProvider(
        target="cloud", checkpoints={"swe:ready": "mach-golden"}
    )
    handle = await provider.create(SandboxSpec(image="swe:ready", ttl_s=12.1))
    assert handle.raw.episode is not None
    assert handle.raw.episode.ttl_secs == 13
    assert handle.raw.ttl_task is None
    assert handle.sandbox_id.startswith("lease-nemo-gym-")
    await provider.close(handle)
    assert handle.raw.episode.completed == ["done"]


@pytest.mark.asyncio
async def test_checkpoint_never_silently_ignores_incompatible_spec() -> None:
    provider = adapter.SmolProvider(
        checkpoints={"swe:ready": {"machine": "golden", "ports": [8000]}},
    )
    with pytest.raises(ValueError, match="entrypoint must be omitted or exactly match"):
        await provider.create(SandboxSpec(image="swe:ready", entrypoint=["wrong"]))
    with pytest.raises(ValueError, match="does not declare requested port"):
        await provider.create(SandboxSpec(image="swe:ready", ports=[9000]))
    with pytest.raises(ValueError, match="declare checkpoint.resources"):
        await provider.create(
            SandboxSpec(image="swe:ready", resources={"memory_mib": 1024})
        )
    with pytest.raises(ValueError, match="inherits its checkpoint's network policy"):
        await provider.create(
            SandboxSpec(
                image="swe:ready",
                provider_options={"allow_hosts": ["api.example.com"]},
            )
        )
    assert FakeMachine.created == []
    assert FakeMachine.connected == []


@pytest.mark.asyncio
async def test_checkpoint_accepts_declared_capacity_and_exact_network_policy() -> None:
    checkpoint = {
        "shared:ready": {
            "machine": "golden",
            "resources": {
                "cpu": 4,
                "memory_mib": 8192,
                "disk_gib": 20,
                "gpu": 1,
                "gpu_type": "H100",
            },
            "provider_options": {
                "allow_hosts": ["api.example.com"],
                "allow_cidrs": ["10.1.2.3/8"],
            },
        }
    }
    provider = adapter.SmolProvider(target="cloud", checkpoints=checkpoint)
    handle = await provider.create(
        SandboxSpec(
            image="shared:ready",
            resources={
                "cpu": 2,
                "memory_mib": 4096,
                "disk_gib": 10,
                "gpu": 1,
                "gpu_type": "H100",
            },
            provider_options={
                "allow_cidrs": ["10.0.0.0/8"],
                "allow_hosts": ["https://api.example.com:443"],
            },
        )
    )
    await provider.close(handle)

    with pytest.raises(ValueError, match="memory_mib=8192"):
        await provider.create(
            SandboxSpec(
                image="shared:ready",
                resources={"memory_mib": 16384},
                provider_options=checkpoint["shared:ready"]["provider_options"],
            )
        )


@pytest.mark.asyncio
async def test_create_failure_deletes_partial_machine(monkeypatch) -> None:
    provider = adapter.SmolProvider()

    async def bad_exec(self, command, options=None):
        return ExecResult(exit_code=1, stdout="", stderr="not ready")

    monkeypatch.setattr(FakeMachine, "exec", bad_exec)
    with pytest.raises(
        adapter.SandboxCreateVerificationError, match="failed readiness probe"
    ):
        await provider.create(SandboxSpec(image="alpine:3.20"))
    assert FakeMachine.instances[-1].deleted is True


@pytest.mark.asyncio
async def test_user_and_closed_execution_behavior() -> None:
    provider = adapter.SmolProvider()
    handle = await provider.create(SandboxSpec(image="swe:ready"))
    await provider.exec(handle, "id", user="sandbox")
    command = handle.raw.machine.commands[-1][0][-1]
    assert "exec su -s /bin/sh sandbox -c id" in command
    await provider.close(handle)
    result = await provider.exec(handle, "true")
    assert result.return_code == 125 and result.error_type == "sandbox"


def test_configuration_is_strict() -> None:
    with pytest.raises(ValueError, match="target"):
        adapter.SmolProvider(target="somewhere")
    with pytest.raises(ValueError, match="Unknown checkpoint keys"):
        adapter.SmolProvider(checkpoints={"img": {"machine": "m", "typo": True}})
    with pytest.raises(ValueError, match="integer TCP ports"):
        adapter.SmolProvider(checkpoints={"img": {"machine": "m", "ports": [True]}})
    with pytest.raises(TypeError, match="allow_hosts must be a list"):
        adapter.SmolProvider(
            checkpoints={
                "img": {
                    "machine": "m",
                    "provider_options": {"allow_hosts": "example.com"},
                }
            }
        )
    with pytest.raises(ValueError, match="fork_batch_window_ms"):
        adapter.SmolProvider(fork_batch_window_ms=float("nan"))
    provider = adapter.SmolProvider()
    with pytest.raises(ValueError, match="Unknown smol provider option"):
        provider._options(SandboxSpec(provider_options={"typo": True}))


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("field", "value"),
    [("ready_timeout_s", 0), ("ready_timeout_s", float("nan")), ("ttl_s", 0)],
)
async def test_create_rejects_invalid_lifecycle_timeouts(field, value) -> None:
    provider = adapter.SmolProvider()
    with pytest.raises(adapter.SandboxCreateError, match="finite and > 0"):
        await provider.create(SandboxSpec(image="alpine", **{field: value}))
    assert FakeMachine.created == []


@pytest.mark.asyncio
async def test_cold_create_rejects_invalid_resources_and_network() -> None:
    provider = adapter.SmolProvider()
    with pytest.raises(adapter.SandboxCreateError, match="memory_mib"):
        await provider.create(SandboxSpec(image="alpine", resources={"memory_mib": -1}))
    with pytest.raises(adapter.SandboxCreateError, match="Invalid allowed CIDR"):
        await provider.create(
            SandboxSpec(
                image="alpine",
                provider_options={"allow_cidrs": ["not-a-cidr"]},
            )
        )
    assert FakeMachine.created == []


def test_machine_config_validates_new_creation_controls() -> None:
    from smol import MachineConfig

    with pytest.raises(ValueError, match="ready_timeout_seconds"):
        MachineConfig(ready_timeout_seconds=0)
    with pytest.raises(ValueError, match="command"):
        MachineConfig(command=[])


@pytest.mark.asyncio
async def test_local_ttl_deletes_machine() -> None:
    provider = adapter.SmolProvider(probe_command=None)
    handle = await provider.create(SandboxSpec(image="alpine", ttl_s=0.01))
    await asyncio.sleep(0.03)
    assert handle.raw.closed is True
    assert handle.raw.machine.deleted is True


@pytest.mark.asyncio
async def test_local_ttl_retries_transient_delete_failure(monkeypatch) -> None:
    provider = adapter.SmolProvider(probe_command=None)
    handle = await provider.create(SandboxSpec(image="alpine", ttl_s=0.01))
    original_delete = handle.raw.machine.delete
    attempts = 0

    async def flaky_delete():
        nonlocal attempts
        attempts += 1
        if attempts < 3:
            raise RuntimeError("temporary delete failure")
        await original_delete()

    monkeypatch.setattr(handle.raw.machine, "delete", flaky_delete)
    await asyncio.sleep(0.35)
    assert attempts == 3
    assert handle.raw.closed is True
    assert handle.raw.machine.deleted is True


@pytest.mark.asyncio
async def test_official_async_sandbox_drives_provider_end_to_end() -> None:
    provider = adapter.SmolProvider(probe_command=None)
    sandbox = AsyncSandbox(
        provider,
        SandboxSpec(
            image="python:3.12",
            workdir="/workspace",
            files={"/workspace/task.txt": "solve me"},
        ),
    )
    await sandbox.start()
    assert await sandbox.status() is SandboxStatus.RUNNING
    result = await sandbox.exec("cat task.txt")
    assert result.return_code == 0
    assert sandbox._handle.raw.machine.files["/workspace/task.txt"] == b"solve me"
    await sandbox.stop()
    assert await sandbox.status() is SandboxStatus.STOPPED


@pytest.mark.asyncio
async def test_provider_aclose_does_not_delete_other_live_episodes() -> None:
    provider = adapter.SmolProvider(probe_command=None)
    first = await provider.create(SandboxSpec(image="alpine"))
    second = await provider.create(SandboxSpec(image="alpine"))
    await provider.close(first)
    await provider.aclose()
    assert first.raw.machine.deleted is True
    assert second.raw.machine.deleted is False
    await provider.close(second)


@pytest.mark.asyncio
async def test_failed_close_remains_retryable(monkeypatch) -> None:
    provider = adapter.SmolProvider(probe_command=None)
    handle = await provider.create(SandboxSpec(image="alpine"))
    original_delete = handle.raw.machine.delete
    attempts = 0

    async def flaky_delete():
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise RuntimeError("temporary delete failure")
        await original_delete()

    monkeypatch.setattr(handle.raw.machine, "delete", flaky_delete)
    with pytest.raises(RuntimeError, match="temporary delete failure"):
        await provider.close(handle)
    assert handle.raw.closed is False
    await provider.close(handle)
    assert handle.raw.closed is True


@pytest.mark.asyncio
async def test_handle_serializes_without_credentials_and_reconnects() -> None:
    first_provider = adapter.SmolProvider(target="cloud", api_key="secret")
    assert isinstance(first_provider, ConnectableProvider)
    sandbox = AsyncSandbox(
        first_provider,
        SandboxSpec(
            image="alpine",
            workdir="/workspace",
            env={"EPISODE": "7"},
            ports=[8080],
        ),
    )
    await sandbox.start()
    descriptor = await sandbox.serialize(scope="operate")
    assert descriptor == {
        "sandbox_id": sandbox._handle.sandbox_id,
        "env": {"EPISODE": "7"},
        "workdir": "/workspace",
        "ports": [8080],
    }
    assert "secret" not in repr(descriptor)

    second_provider = adapter.SmolProvider(target="cloud", api_key="other-secret")
    attached = await AsyncSandbox.connect(descriptor, provider=second_provider)
    result = await attached.exec("printf $EPISODE")
    assert result.return_code == 0
    _, options = attached._handle.raw.machine.commands[-1]
    assert options.env == {"EPISODE": "7"}
    assert options.workdir == "/workspace"
    await attached.stop()
    # The attached owner deleted the machine; mark the original facade stopped
    # so its finalizer does not attempt a second lifecycle operation.
    sandbox._stopped = True
    await sandbox._provider.aclose()


@pytest.mark.asyncio
async def test_concurrent_provider_instances_coalesce_checkpoint_forks() -> None:
    config = {"shared:ready": "golden-batch"}
    providers = [
        adapter.SmolProvider(checkpoints=config, fork_batch_window_ms=20)
        for _ in range(8)
    ]
    handles = await asyncio.gather(
        *(provider.create(SandboxSpec(image="shared:ready")) for provider in providers)
    )
    goldens = [
        machine for machine in FakeMachine.instances if machine.id == "golden-batch"
    ]
    assert len(goldens) == 1
    assert len(goldens[0].fork_batches) == 1
    assert len(goldens[0].fork_batches[0]) == 8
    await asyncio.gather(
        *(
            provider.close(handle)
            for provider, handle in zip(providers, handles)  # noqa: B905 - Python 3.9
        )
    )
