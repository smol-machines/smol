"""Contract tests for the Ray Sandboxes backend."""

from __future__ import annotations

import json
import os
from typing import ClassVar

import pytest

pytest.importorskip("ray.experimental.sandbox")

import smol.ray_sandbox as adapter
from ray.experimental.sandbox.backend.base import SandboxStatus
from ray.experimental.sandbox.config import SandboxConfig
from ray.experimental.sandbox.exceptions import (
    SandboxNotFoundError,
    SandboxTimeoutError,
)
from smol.types import ConnectOptions, PortableCheckpointInfo
from smol.types import ExecResult as SmolExecResult


class FakeMachine:
    created: ClassVar[list[tuple[object, object]]] = []
    connected: ClassVar[list[str]] = []
    restored: ClassVar[list[tuple[str, str, object]]] = []

    def __init__(self, name: str) -> None:
        self.id = f"mach-{name}"
        self.name = name
        self.commands: list[tuple[list[str], object]] = []
        self.files: dict[str, bytes] = {}
        self.batches: list[list[str]] = []
        self.paused = False
        self.stopped = False
        self.deleted = False
        self.exit_code = 0

    @classmethod
    def create(cls, config, conn):
        cls.created.append((config, conn))
        return cls(config.name)

    @classmethod
    def connect(cls, machine_id, conn):
        cls.connected.append(machine_id)
        return cls(machine_id)

    @classmethod
    def restore_checkpoint(cls, checkpoint_id, name, conn, network=None):
        cls.restored.append((checkpoint_id, name, network))
        return cls(name)

    def exec(self, command, options=None):
        self.commands.append((list(command), options))
        return SmolExecResult(exit_code=self.exit_code, stdout="out", stderr="",
                              stdout_bytes=b"out")

    def write_file(self, path, data, mode=None):
        self.files[path] = bytes(data)

    def read_file(self, path):
        return self.files[path]

    def state(self):
        if self.stopped:
            return "stopped"
        return "paused" if self.paused else "running"

    def pause(self):
        self.paused = True

    def resume(self, wait_until_ready=True):
        self.paused = False

    def delete(self):
        self.deleted = True

    def checkpoint(self, output=None, *, store=None):
        if output:
            with open(output, "wb") as f:
                f.write(b"artifact")
        return PortableCheckpointInfo(
            id="ckpt-1", machine_id=self.id, status="ready", size_bytes=8,
            arch="x86_64", created_at="", download_url="",
        )

    def branch_batch(self, count=None, *, names=None, **kwargs):
        self.batches.append(list(names))
        return [FakeMachine(n) for n in names]


@pytest.fixture(autouse=True)
def fake_sdk(monkeypatch):
    FakeMachine.created.clear()
    FakeMachine.connected.clear()
    FakeMachine.restored.clear()
    monkeypatch.setattr(adapter, "Machine", FakeMachine)
    monkeypatch.delenv(adapter.TARGET_ENV, raising=False)


def local_backend():
    return adapter.SmolSandboxBackend(conn=ConnectOptions(target="local"))


def test_create_maps_ray_config_to_a_branchable_persistent_machine():
    backend = local_backend()
    sid = backend.create_sandbox(
        SandboxConfig(image="python:3.12-slim", cpu=1.5, memory="2Gi", network="none",
                      env={"A": "1"}, workdir="/workspace")
    )
    config, conn = FakeMachine.created[0]
    assert sid == config.name and sid.startswith("ray-sb-")
    assert config.image == "python:3.12-slim"
    assert config.resources.cpus == 2 and config.resources.memory_mb == 2048
    assert config.network is False
    assert config.branchable and config.persistent
    assert config.env == {"A": "1"} and config.workdir == "/workspace"
    assert conn.target == "local"
    # An explicit workdir exists before the first command runs in it.
    assert backend._machines[sid].commands[0][0] == ["mkdir", "-p", "/workspace"]


def test_any_ray_network_mode_but_none_enables_egress():
    backend = local_backend()
    backend.create_sandbox(SandboxConfig(image="alpine", network="public"))
    assert FakeMachine.created[0][0].network is True


def test_string_commands_run_through_the_configured_shell_in_the_workdir():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine", workdir="/w", shell="/bin/sh"))
    result = backend.exec_command(sid, "echo hi", env={"X": "y"})
    argv, options = backend._machines[sid].commands[-1]
    assert argv == ["/bin/sh", "-c", "echo hi"]
    assert options.workdir == "/w" and options.env == {"X": "y"}
    assert result.exit_code == 0 and result.stdout == "out"
    backend.exec_command(sid, ["ls", "-l"], cwd="/tmp")
    argv, options = backend._machines[sid].commands[-1]
    assert argv == ["ls", "-l"] and options.workdir == "/tmp"


def test_exec_that_hits_its_timeout_raises_ray_timeout(monkeypatch):
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    backend._machines[sid].exit_code = 124
    clock = iter([0.0, 5.0])
    monkeypatch.setattr(adapter.time, "time", lambda: next(clock))
    with pytest.raises(SandboxTimeoutError):
        backend.exec_command(sid, "sleep 60", timeout=2)


def test_files_append_and_relative_paths_resolve_against_the_workdir():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine", workdir="/w"))
    backend.write_file(sid, "a.txt", "one")
    backend.write_file(sid, "a.txt", b"two", append=True)
    assert backend.read_file(sid, "/w/a.txt") == b"onetwo"


def test_status_pause_resume_and_delete():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    assert backend.get_status(sid) is SandboxStatus.RUNNING
    backend.pause_sandbox(sid)
    assert backend.get_status(sid) is getattr(SandboxStatus, "PAUSED", SandboxStatus.RUNNING)
    backend.resume_sandbox(sid)
    machine = backend._machines[sid]
    backend.delete_sandbox(sid)
    assert machine.deleted
    assert backend.get_status(sid) is SandboxStatus.TERMINATED
    with pytest.raises(SandboxNotFoundError):
        backend.exec_command(sid, "true")


def test_restore_ignores_manifest_fields_this_ray_does_not_know(tmp_path):
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    path = backend.checkpoint_sandbox(sid, str(tmp_path / "ck"))["checkpoint_path"]
    manifest_file = os.path.join(path, "manifest.json")
    manifest = json.load(open(manifest_file))
    manifest["config"]["_ignore_cgroups"] = False
    manifest["config"]["field_from_a_newer_ray"] = 1
    json.dump(manifest, open(manifest_file, "w"))
    assert backend.restore_sandbox(path) in backend._machines


def test_local_checkpoint_is_a_ray_bundle_that_restores_from_its_artifact(tmp_path):
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine", workdir="/w"))
    out = backend.checkpoint_sandbox(sid, str(tmp_path / "ck"))
    path = out["checkpoint_path"]
    manifest = json.load(open(os.path.join(path, "manifest.json")))
    assert manifest["backend"] == "smol" and manifest["smol_target"] == "local"
    assert os.path.isfile(os.path.join(path, "state", "sandbox.smolcheckpoint"))
    restored = backend.restore_sandbox(path)
    source, name, network = FakeMachine.restored[0]
    assert source == os.path.join(path, "state", "sandbox.smolcheckpoint")
    assert restored == name and network is False
    assert backend._sandbox_metadata[restored]["workdir"] == "/w"


def test_checkpoint_without_leave_running_deletes_the_source(tmp_path):
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    machine = backend._machines[sid]
    backend.checkpoint_sandbox(sid, str(tmp_path / "ck"), leave_running=False)
    assert machine.deleted


def test_cloud_checkpoint_restores_by_id_without_moving_the_artifact(tmp_path):
    backend = adapter.SmolSandboxBackend(conn=ConnectOptions(target="cloud"))
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    assert sid.startswith("mach-")
    path = backend.checkpoint_sandbox(sid, str(tmp_path / "ck"))["checkpoint_path"]
    assert not os.listdir(os.path.join(path, "state"))
    backend.restore_sandbox(path)
    assert FakeMachine.restored[0][0] == "ckpt-1"


def test_branches_come_from_one_batch_and_can_be_attached_elsewhere():
    backend = local_backend()
    parent = backend.create_sandbox(SandboxConfig(image="alpine", workdir="/w"))
    kids = backend.branch_sandboxes(parent, 3)
    assert backend._machines[parent].batches == [kids]
    assert all(k in backend._machines for k in kids)
    other = local_backend()
    sid = other.attach_sandbox(kids[0], "/w")
    assert sid == kids[0] and FakeMachine.connected == [kids[0]]
    assert other._sandbox_metadata[sid]["workdir"] == "/w"


def test_target_comes_from_the_environment(monkeypatch):
    monkeypatch.setenv(adapter.TARGET_ENV, "cloud")
    assert adapter.SmolSandboxBackend().target == "cloud"
    monkeypatch.setenv(adapter.TARGET_ENV, "elsewhere")
    with pytest.raises(ValueError):
        adapter.SmolSandboxBackend()


def test_exec_accepts_arguments_a_newer_ray_passes():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    result = backend.exec_command(sid, ["true"], argument_from_a_newer_ray=1)
    assert result.exit_code == 0


@pytest.mark.skipif(
    "output" not in adapter.ExecResult.__dataclass_fields__,
    reason="this Ray has no combined exec output",
)
def test_combine_output_returns_one_stream_in_output():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    result = backend.exec_command(sid, ["make", "test"], combine_output=True)
    argv, _ = backend._machines[sid].commands[-1]
    # stderr is merged into stdout inside the machine, so the order is kept.
    assert argv == ["/bin/sh", "-c", 'exec "$@" 2>&1', "sh", "make", "test"]
    assert result.output == b"out"
    assert result.stdout == "" and result.stderr == ""


def test_checkpoint_manifest_holds_only_fields_this_ray_defines(tmp_path):
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine", workdir="/w"))
    path = backend.checkpoint_sandbox(sid, str(tmp_path / "ck"))["checkpoint_path"]
    saved = json.load(open(os.path.join(path, "manifest.json")))["config"]
    fields = {f for f in SandboxConfig.__dataclass_fields__ if not f.startswith("_")}
    assert set(saved) <= fields
    assert saved["image"] == "alpine" and saved["workdir"] == "/w"


def test_termination_reason_only_for_an_unexpected_stop():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    assert backend.get_termination_reason(sid) is None
    backend._machines[sid].stopped = True
    assert backend.get_termination_reason(sid)
    backend.delete_sandbox(sid)
    assert backend.get_termination_reason(sid) is None
    assert backend.get_termination_reason("never-created") is None


def test_config_scratch_dir_and_mounts_of_an_owned_sandbox():
    backend = local_backend()
    sid = backend.create_sandbox(SandboxConfig(image="alpine"))
    assert backend.get_config(sid).image == "alpine"
    assert os.path.isdir(backend.sandbox_dir(sid))
    assert backend.mount_points(sid) == []
    with pytest.raises(SandboxNotFoundError):
        backend.get_config("never-created")


def test_commit_is_refused_before_anything_is_paused():
    from ray.experimental.sandbox.exceptions import SandboxError

    runtime = adapter._runtime_class()()
    sid = runtime._backend.create_sandbox(SandboxConfig(image="alpine"))
    with pytest.raises(SandboxError, match="not supported by the smol backend"):
        runtime.commit(sid, "registry.example/x:y")
    assert not runtime._backend._machines[sid].paused
