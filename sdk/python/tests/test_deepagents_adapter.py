"""Deep Agents sandbox contract against a machine-shaped transport."""

from __future__ import annotations

import base64

import pytest

pytest.importorskip("deepagents")

from deepagents.backends.sandbox import BaseSandbox
from smol.deepagents import SmolSandbox
from smol.errors import SmolError
from smol.machine import Machine
from smol.transport import CloudTransport
from smol.types import ExecResult


class FakeMachine:
    id = "smol-test"

    def __init__(self) -> None:
        self.files: dict[str, bytes] = {}
        self.commands: list[tuple[list[str], object]] = []

    def exec(self, command: list[str], opts: object = None) -> ExecResult:
        self.commands.append((command, opts))
        if command[0] == "mkdir":
            return ExecResult(exit_code=0, stdout="", stderr="")
        return ExecResult(
            exit_code=7, stdout="standard", stderr="error", stderr_truncated=True
        )

    def read_file(self, path: str) -> bytes:
        if path == "/workspace":
            raise SmolError("SMOLVM_ERROR", "is a directory: /workspace")
        if path not in self.files:
            raise SmolError(
                "SMOLVM_ERROR", "open path: No such file or directory (os error 2)"
            )
        return self.files[path]

    def write_file(self, path: str, content: bytes) -> None:
        self.files[path] = content


def test_sandbox_executes_in_guest_with_timeout_and_reports_truncation() -> None:
    machine = FakeMachine()
    backend = SmolSandbox(machine)  # type: ignore[arg-type]
    assert isinstance(backend, BaseSandbox)
    assert backend.id == "smol-test"
    result = backend.execute("echo hello", timeout=4)
    assert (result.output, result.exit_code, result.truncated) == (
        "standard\nerror",
        7,
        True,
    )
    command, options = machine.commands[0]
    assert command == ["/bin/sh", "-lc", "echo hello"]
    assert options.timeout == 4  # type: ignore[attr-defined]
    assert options.output == "b64"  # type: ignore[attr-defined]
    with pytest.raises(ValueError, match="timeout"):
        backend.execute("echo hello", timeout=0)


def test_sandbox_transfers_binary_files_and_reports_missing_or_relative_paths() -> None:
    backend = SmolSandbox(FakeMachine())  # type: ignore[arg-type]
    uploaded = backend.upload_files(
        [("/workspace/nested/test.bin", b"\x00\xff"), ("relative", b"bad")]
    )
    assert backend.machine.commands == [(["mkdir", "-p", "/workspace/nested"], None)]
    assert [response.error for response in uploaded] == [None, "invalid_path"]
    downloaded = backend.download_files(
        ["/workspace/nested/test.bin", "/workspace/missing", "/workspace", "relative"]
    )
    assert [(response.content, response.error) for response in downloaded] == [
        (b"\x00\xff", None),
        (None, "file_not_found"),
        (None, "is_directory"),
        (None, "invalid_path"),
    ]


def test_cloud_byte_output_restores_complete_commands_and_reports_real_truncation() -> (
    None
):
    class CloudLikeMachine(FakeMachine):
        def exec(self, command: list[str], opts: object = None) -> ExecResult:
            self.commands.append((command, opts))
            return ExecResult(
                exit_code=0,
                stdout="initial stdout",
                stdout_bytes=b"initial stdout and more",
                stdout_truncated=True,
                stderr="initial stderr",
                stderr_bytes=b"initial stderr",
                stderr_truncated=True,
            )

    backend = SmolSandbox(CloudLikeMachine())  # type: ignore[arg-type]
    result = backend.execute("echo lots")
    assert result.output == "initial stdout and more\ninitial stderr"
    assert result.truncated is True  # older controls can only return capped stderr


def test_cloud_transport_sends_b64_and_recovers_complete_output(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def cloud_fetch(
        base: str, key: str, method: str, path: str, **kwargs: object
    ) -> dict:
        assert (base, key, method) == ("https://example.test", "token", "POST")
        assert path == "/v1/machines/mach-123/exec?output=b64"
        assert kwargs["json_body"]["command"] == ["/bin/sh", "-lc", "cat file"]  # type: ignore[index]
        return {
            "stdout": "capped",
            "stdoutTruncated": True,
            "stdoutB64": base64.b64encode(b"complete command result").decode(),
            "stderr": "",
            "stderrB64": "",
            "exitCode": 0,
        }

    monkeypatch.setattr("smol.transport._cloud_fetch", cloud_fetch)
    machine = Machine(CloudTransport("https://example.test", "token", "mach-123", "vm"))
    result = SmolSandbox(machine).execute("cat file")
    assert result.output == "complete command result"
    assert result.truncated is False
