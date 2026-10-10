"""smolagents contract tests; opt in to the real VM path for local integration."""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

import pytest

pytest.importorskip("smolagents")

from smol._smolagents_worker import _receive
from smol.machine import Machine
from smol.smolagents import SmolExecutor


def _round_trip(socket_path: Path, request: dict) -> dict:
    payload = json.dumps(request).encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.connect(str(socket_path))
        conn.sendall(len(payload).to_bytes(8, "big") + payload)
        header = bytearray()
        while len(header) < 8:
            header.extend(conn.recv(8 - len(header)))
        length = int.from_bytes(header, "big")
        result = bytearray()
        while len(result) < length:
            result.extend(conn.recv(length - len(result)))
    return json.loads(result)


def test_worker_preserves_state_and_recovers_after_errors(tmp_path: Path) -> None:
    source = Path(__file__).resolve().parents[1] / "python/smol/_smolagents_worker.py"
    worker = tmp_path / "worker.py"
    shutil.copyfile(source, worker)
    path = tmp_path / "smol.sock"
    process = subprocess.Popen([sys.executable, str(worker), str(path)])
    try:
        for _ in range(100):
            if path.exists():
                break
            time.sleep(0.01)
        assert path.exists()
        assert (
            _round_trip(path, {"code": 'answer = 1024\nprint("stored")'})["logs"]
            == "stored\n"
        )
        assert _round_trip(path, {"code": "answer"})["result"] == "1024"
        assert (
            "ValueError: broken"
            in _round_trip(path, {"code": 'raise ValueError("broken")'})["error"]
        )
        assert _round_trip(path, {"code": "answer"})["result"] == "1024"
        assert (
            "timed out"
            in _round_trip(path, {"code": "while True: pass", "timeout": 1})["error"]
        )
        assert _round_trip(path, {"code": "answer"})["result"] == "1024"
        assert _round_trip(path, {"shutdown": True})["result"] is None
        assert process.wait(timeout=5) == 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)


def test_worker_rejects_oversize_request() -> None:
    first, second = socket.socketpair()
    try:
        second.sendall((9 * 1024 * 1024).to_bytes(8, "big"))
        with pytest.raises(ValueError, match="size limit"):
            _receive(first)
    finally:
        first.close()
        second.close()


def test_branchable_option_reaches_created_machine(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class CreationIntercept(Exception):
        pass

    captured = []

    def capture_create(config, conn):
        captured.append((config, conn))
        raise CreationIntercept()

    monkeypatch.setattr(Machine, "create", capture_create)
    with pytest.raises(CreationIntercept):
        SmolExecutor(target="cloud", branchable=True)
    config, conn = captured[0]
    assert config.branchable is True
    assert config.forkable is True  # compatibility field sent to the Cloud API
    assert conn.target == "cloud"
    with pytest.raises(TypeError, match="branchable must be a boolean"):
        SmolExecutor(branchable=1)  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="caller-owned machine"):
        SmolExecutor(machine=object(), branchable=True)  # type: ignore[arg-type]


@pytest.mark.skipif(
    os.environ.get("SMOL_SMOLAGENTS_INTEGRATION") != "1",
    reason="opt-in local microVM test",
)
def test_codeagent_runs_final_answer_in_live_vm() -> None:
    from smolagents import CodeAgent
    from smolagents.models import ChatMessage, MessageRole, Model

    class TwoStepModel(Model):
        def __init__(self):
            super().__init__()
            self.calls = 0

        def generate(self, messages, stop_sequences=None, **kwargs):
            self.calls += 1
            code = "stored = 1000 + 24" if self.calls == 1 else "final_answer(stored)"
            return ChatMessage(
                role=MessageRole.ASSISTANT, content=f"<code>\n{code}\n</code>"
            )

    executor = SmolExecutor(timeout_seconds=30)
    try:
        executor.send_variables({"project_id": "agent-42"})
        assert executor("project_id").output == "'agent-42'"
        with CodeAgent(
            tools=[], model=TwoStepModel(), executor=executor, max_steps=3
        ) as agent:
            assert agent.run("Compute 1000 + 24") == 1024
            assert agent.run("Read the saved result again") == 1024
        with pytest.raises(RuntimeError, match="closed"):
            executor("print('should not run')")
    finally:
        executor.cleanup()


@pytest.mark.skipif(
    os.environ.get("SMOL_SMOLAGENTS_INTEGRATION") != "1",
    reason="opt-in local microVM test",
)
def test_caller_owned_vm_survives_executor_cleanup() -> None:
    from smol import Machine, MachineConfig

    machine = Machine.create(MachineConfig(image="python:3.12-slim"))
    try:
        executor = SmolExecutor(machine=machine)
        assert executor("2 + 2").output == "4"
        executor.cleanup()
        with pytest.raises(ValueError, match="Invalid package requirement"):
            SmolExecutor(machine=machine, additional_imports=["-malicious-option"])
        assert (
            machine.exec(["python3", "-c", "print('alive')"]).stdout.strip() == "alive"
        )
    finally:
        machine.delete()


@pytest.mark.skipif(
    os.environ.get("SMOL_SMOLAGENTS_INTEGRATION") != "1",
    reason="opt-in local microVM test",
)
def test_branchable_executor_preserves_state_through_pause_resume() -> None:
    executor = SmolExecutor(branchable=True)
    try:
        executor("value = 21")
        executor.machine.pause()
        executor.machine.resume()
        assert executor("value * 2").output == "42"
    finally:
        executor.cleanup()
    with pytest.raises(RuntimeError, match="closed"):
        _ = executor.machine
