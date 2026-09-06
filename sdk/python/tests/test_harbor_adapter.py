"""Contract tests for the Harbor environment provider."""

from __future__ import annotations

import asyncio
import tarfile
from pathlib import Path
from typing import ClassVar

import pytest

pytest.importorskip("harbor")

import smol.harbor as adapter
from harbor.models.task.config import EnvironmentConfig, NetworkMode, NetworkPolicy
from harbor.models.trial.paths import TrialPaths
from smol.types import ExecResult as SmolExecResult


class FakeMachine:
    created: ClassVar[list[tuple[object, object]]] = []
    connected: ClassVar[list[tuple[str, object]]] = []
    instances: ClassVar[list[FakeMachine]] = []

    def __init__(self, machine_id: str) -> None:
        self.id = machine_id
        self.name = machine_id
        self.commands: list[tuple[list[str], object]] = []
        self.files: dict[str, bytes] = {}
        self.fork_batches: list[list[str]] = []
        self.deleted = False
        self.stopped = False
        FakeMachine.instances.append(self)

    @classmethod
    async def create(cls, config, conn):
        cls.created.append((config, conn))
        return cls(config.name)

    @classmethod
    async def connect(cls, machine_id, conn):
        cls.connected.append((machine_id, conn))
        return cls(machine_id)

    async def fork_batch(self, *, names):
        self.fork_batches.append(list(names))
        return [FakeMachine(f"fork-{name}") for name in names]

    async def exec(self, command, options=None):
        self.commands.append((list(command), options))
        return SmolExecResult(exit_code=0, stdout="ok", stderr="")

    async def write_file(self, path, data, mode=None):
        self.files[path] = bytes(data)
        self.files[f"{path}:mode"] = str(mode).encode()

    async def read_file(self, path):
        return self.files[path]

    async def delete(self):
        self.deleted = True

    async def stop(self):
        self.stopped = True


@pytest.fixture(autouse=True)
def fake_sdk(monkeypatch):
    FakeMachine.created.clear()
    FakeMachine.connected.clear()
    FakeMachine.instances.clear()
    adapter._LOOP_STATES.clear()
    adapter._OWNED_GOLDENS.clear()
    monkeypatch.setattr(adapter, "AsyncMachine", FakeMachine)
    yield
    adapter._LOOP_STATES.clear()
    adapter._OWNED_GOLDENS.clear()


def _environment(
    tmp_path: Path,
    *,
    session_id: str = "task__trial__env",
    auto_checkpoint: bool = True,
    checkpoints=None,
    network_policy: NetworkPolicy | None = None,
) -> adapter.SmolEnvironment:
    environment_dir = tmp_path / "environment"
    environment_dir.mkdir(exist_ok=True)
    (environment_dir / "Dockerfile").write_text("FROM example/task:latest\n")
    trial_paths = TrialPaths(tmp_path / session_id)
    trial_paths.mkdir()
    return adapter.SmolEnvironment(
        environment_dir=environment_dir,
        environment_name="test-task",
        session_id=session_id,
        trial_paths=trial_paths,
        task_env_config=EnvironmentConfig(
            docker_image="example/task:latest",
            cpus=2,
            memory_mb=2048,
            storage_mb=10240,
            env={"TASK_ENV": "one"},
            workdir="/workspace",
        ),
        persistent_env={"RUN_ENV": "two"},
        network_policy=network_policy or NetworkPolicy(),
        auto_checkpoint=auto_checkpoint,
        checkpoints=checkpoints,
        fork_batch_window_ms=0,
    )


def _dockerfile_environment(
    tmp_path: Path,
    *,
    session_id: str = "dockerfile__trial__env",
    target: str = "local",
) -> adapter.SmolEnvironment:
    environment_dir = tmp_path / session_id / "environment"
    environment_dir.mkdir(parents=True)
    (environment_dir / "Dockerfile").write_text("FROM alpine:3.20\n")
    trial_paths = TrialPaths(tmp_path / session_id / "trial")
    trial_paths.mkdir()
    return adapter.SmolEnvironment(
        environment_dir=environment_dir,
        environment_name="dockerfile-task",
        session_id=session_id,
        trial_paths=trial_paths,
        task_env_config=EnvironmentConfig(
            cpus=1,
            memory_mb=512,
            build_timeout_sec=45,
        ),
        target=target,
        fork_batch_window_ms=0,
    )


@pytest.mark.asyncio
async def test_cold_environment_maps_resources_exec_and_files(tmp_path: Path) -> None:
    env = _environment(tmp_path, auto_checkpoint=False)
    await env.start(force_build=False)

    config, conn = FakeMachine.created[0]
    assert conn.target == "local"
    assert config.image == "example/task:latest"
    assert config.resources.cpus == 2
    assert config.resources.memory_mb == 2048
    assert config.resources.storage_gb == 10
    assert config.resources.network is True
    assert config.env == {"TASK_ENV": "one", "RUN_ENV": "two"}
    assert config.workdir == "/workspace"
    assert config.checkpoint is False
    assert "_" not in config.name

    result = await env.exec(
        "printf $EXTRA", env={"EXTRA": "three"}, timeout_sec=7, user="sandbox"
    )
    assert result.return_code == 0
    command, options = env._machine.commands[-1]
    assert command[-2:] == ["printf $EXTRA", "sandbox"]
    assert options.env == {
        "TASK_ENV": "one",
        "RUN_ENV": "two",
        "EXTRA": "three",
    }
    assert options.workdir == "/workspace"
    assert options.timeout == 7

    source = tmp_path / "run.sh"
    source.write_bytes(b"#!/bin/sh\necho ok\n")
    source.chmod(0o750)
    await env.upload_file(source, "/workspace/bin/run.sh")
    assert env._machine.files["/workspace/bin/run.sh"] == source.read_bytes()
    assert env._machine.files["/workspace/bin/run.sh:mode"] == b"488"
    env._machine.files["/workspace/out"] = b"result"
    target = tmp_path / "download" / "out"
    await env.download_file("/workspace/out", target)
    assert target.read_bytes() == b"result"

    machine = env._machine
    await env.stop(delete=True)
    assert machine.deleted is True


@pytest.mark.asyncio
async def test_auto_checkpoint_is_shared_and_forks_concurrent_trials(
    tmp_path: Path,
) -> None:
    first = _environment(tmp_path, session_id="first__env")
    second = _environment(tmp_path, session_id="second__env")
    await asyncio.gather(first.start(False), second.start(False))

    assert len(FakeMachine.created) == 1
    golden_config, _ = FakeMachine.created[0]
    assert golden_config.checkpoint is True
    golden = next(
        machine for machine in FakeMachine.instances if machine.id == golden_config.name
    )
    assert len(golden.fork_batches) == 1
    assert len(golden.fork_batches[0]) == 2
    assert first._machine.id.startswith("fork-harbor-first-env-")
    assert second._machine.id.startswith("fork-harbor-second-env-")

    first_machine = first._machine
    second_machine = second._machine
    await asyncio.gather(first.stop(True), second.stop(True))
    assert first_machine.deleted and second_machine.deleted
    assert golden.deleted is False
    await adapter.close_harbor_goldens()
    assert golden.deleted is True


@pytest.mark.asyncio
async def test_external_checkpoint_is_validated_and_reused(tmp_path: Path) -> None:
    checkpoints = {
        "example/task:latest": {
            "machine": "prepared-golden",
            "resources": {
                "cpus": 4,
                "memory_mb": 4096,
                "storage_mb": 20480,
                "gpus": 0,
            },
            "network_mode": "public",
        }
    }
    env = _environment(tmp_path, checkpoints=checkpoints)
    await env.start(False)

    assert FakeMachine.created == []
    assert FakeMachine.connected[0][0] == "prepared-golden"
    golden = next(
        machine for machine in FakeMachine.instances if machine.id == "prepared-golden"
    )
    assert len(golden.fork_batches) == 1
    await env.stop(True)

    too_small = {
        "example/task:latest": {
            **checkpoints["example/task:latest"],
            "resources": {"cpus": 1, "memory_mb": 1024, "storage_mb": 1024},
        }
    }
    rejected = _environment(
        tmp_path,
        session_id="rejected__env",
        checkpoints=too_small,
    )
    with pytest.raises(ValueError, match="cannot satisfy requested cpus"):
        await rejected.start(False)

    undeclared_network = {
        "example/task:latest": {
            key: value
            for key, value in checkpoints["example/task:latest"].items()
            if key != "network_mode"
        }
    }
    rejected_network = _environment(
        tmp_path,
        session_id="rejected-network__env",
        checkpoints=undeclared_network,
    )
    with pytest.raises(ValueError, match="network_mode must declare"):
        await rejected_network.start(False)


def test_network_and_definition_validation(tmp_path: Path) -> None:
    allowlist = _environment(
        tmp_path,
        network_policy=NetworkPolicy(
            network_mode=NetworkMode.ALLOWLIST,
            allowed_hosts=["api.example.com", "192.0.2.4", "10.0.0.0/8"],
        ),
    )
    network, hosts, cidrs = allowlist._smol_network()
    assert network is True
    assert hosts == ["api.example.com"]
    assert cidrs == ["192.0.2.4/32", "10.0.0.0/8"]

    with pytest.raises(ValueError, match="wildcard hostnames"):
        _environment(
            tmp_path,
            session_id="wildcard__env",
            network_policy=NetworkPolicy(
                network_mode=NetworkMode.ALLOWLIST,
                allowed_hosts=["*.example.com"],
            ),
        )

    environment_dir = tmp_path / "no-definition" / "environment"
    environment_dir.mkdir(parents=True)
    trial_paths = TrialPaths(tmp_path / "no-definition" / "trial")
    trial_paths.mkdir()
    with pytest.raises(ValueError, match="docker_image or an environment/Dockerfile"):
        adapter.SmolEnvironment(
            environment_dir=environment_dir,
            environment_name="no-definition",
            session_id="no-definition",
            trial_paths=trial_paths,
            task_env_config=EnvironmentConfig(),
        )

    with pytest.raises(ValueError, match="publish the image"):
        _dockerfile_environment(tmp_path, target="cloud")


@pytest.mark.asyncio
async def test_dockerfile_task_builds_archive_for_local_smol(
    tmp_path: Path, monkeypatch
) -> None:
    archive = tmp_path / "cache" / "task.tar"
    archive.parent.mkdir()
    archive.write_bytes(b"docker archive")
    calls = []

    async def prepare(**kwargs):
        calls.append(kwargs)
        return str(archive)

    monkeypatch.setattr(adapter, "_prepare_local_dockerfile_image", prepare)
    env = _dockerfile_environment(tmp_path)
    await env.start(force_build=False)

    assert calls == [
        {
            "environment_dir": env.environment_dir,
            "environment_id": env.environment_id,
            "force_build": False,
            "timeout_sec": 45.0,
        }
    ]
    golden_config, _ = FakeMachine.created[0]
    assert golden_config.image == str(archive)
    await env.stop(True)


@pytest.mark.asyncio
async def test_force_build_uses_dockerfile_instead_of_prebuilt_image(
    tmp_path: Path, monkeypatch
) -> None:
    archive = tmp_path / "rebuilt.tar"
    archive.write_bytes(b"rebuilt")
    force_values = []

    async def prepare(**kwargs):
        force_values.append(kwargs["force_build"])
        return str(archive)

    monkeypatch.setattr(adapter, "_prepare_local_dockerfile_image", prepare)
    env = _environment(tmp_path, auto_checkpoint=False)
    await env.start(force_build=True)

    assert force_values == [True]
    config, _ = FakeMachine.created[0]
    assert config.image == str(archive)

    missing = _environment(
        tmp_path,
        session_id="missing-dockerfile__env",
        auto_checkpoint=False,
    )
    (missing.environment_dir / "Dockerfile").unlink()
    with pytest.raises(ValueError, match="force_build requires"):
        await missing.start(force_build=True)


@pytest.mark.asyncio
async def test_dockerfile_archive_cache_is_atomic_and_content_addressed(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setenv("SMOL_HARBOR_IMAGE_CACHE", str(tmp_path / "image-cache"))
    image_id = None
    builds = 0
    exports = 0

    async def docker_output(*args, **kwargs):
        assert args[:2] == ("version", "--format={{.Server.Os}}/{{.Server.Arch}}")
        return "linux/amd64"

    async def docker_image_id(tag):
        assert tag.startswith("smol-harbor:")
        return image_id

    async def run_logged(*args, log_path, timeout_sec):
        nonlocal image_id, builds, exports
        assert timeout_sec == 90
        if args[0] == "build":
            builds += 1
            image_id = f"sha256:image-{builds}"
            return
        assert args[:2] == ("image", "save")
        exports += 1
        output = Path(
            next(
                arg.removeprefix("--output=")
                for arg in args
                if arg.startswith("--output=")
            )
        )
        with tarfile.open(output, "w") as archive:
            payload = tmp_path / "payload"
            payload.write_text(image_id or "missing")
            archive.add(payload, arcname="payload")

    monkeypatch.setattr(adapter, "_docker_output", docker_output)
    monkeypatch.setattr(adapter, "_docker_image_id", docker_image_id)
    monkeypatch.setattr(adapter, "_run_docker_logged", run_logged)
    environment = tmp_path / "context"
    environment.mkdir()
    (environment / "Dockerfile").write_text("FROM scratch\n")

    first, second = await asyncio.gather(
        *(
            adapter._prepare_local_dockerfile_image(
                environment_dir=environment,
                environment_id="abc123",
                force_build=False,
                timeout_sec=90,
            )
            for _ in range(2)
        )
    )
    assert first == second
    assert Path(first).is_file()
    assert builds == 1
    assert exports == 1
    assert not list((tmp_path / "image-cache").rglob("*.tmp"))

    rebuilt = await adapter._prepare_local_dockerfile_image(
        environment_dir=environment,
        environment_id="abc123",
        force_build=True,
        timeout_sec=90,
    )
    assert rebuilt != first
    assert builds == 2
    assert exports == 2
