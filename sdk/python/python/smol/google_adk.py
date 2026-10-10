"""Run Google ADK model-generated Python in session-isolated Smol VMs.

Install ``smolmachines[google-adk]``. Close the executor when its owning
application stops so local and Cloud VMs are deleted.
"""

from __future__ import annotations

import threading
from types import TracebackType
from typing import Any, Self

from google.adk.code_executors import BaseCodeExecutor
from google.adk.code_executors.code_execution_utils import (
    CodeExecutionInput,
    CodeExecutionResult,
)
from pydantic import Field, PrivateAttr

from .machine import Machine
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

_MAX_OUTPUT = 50 * 1024
_MAX_CODE = 64 * 1024


class _SessionMachine:
    def __init__(self) -> None:
        self.machine: Machine | None = None
        self.lock = threading.RLock()
        self.closed = False


class SmolCodeExecutor(BaseCodeExecutor):
    """Execute ADK code in a separate local or Cloud VM for each session.

    The same session retains its guest filesystem across code blocks; Python
    globals do not carry across calls. Guest egress is disabled by default.
    Call ``close()`` or use a context manager to delete all owned VMs.
    """

    stateful: bool = Field(default=False, frozen=True, exclude=True)
    optimize_data_file: bool = Field(default=False, frozen=True, exclude=True)
    image: str = Field(default="python:3.12-alpine", min_length=1)
    target: str = Field(default="local", pattern="^(local|cloud)$")
    timeout_seconds: int = Field(default=30, ge=1, le=300)
    cloud_ttl_seconds: int = Field(default=3600, ge=60)

    _connection: ConnectOptions = PrivateAttr()
    _resources: ResourceSpec = PrivateAttr()
    _sessions: dict[tuple[str, str, str], _SessionMachine] = PrivateAttr(
        default_factory=dict
    )
    _lock: threading.RLock = PrivateAttr(default_factory=threading.RLock)
    _closed: bool = PrivateAttr(default=False)

    def __init__(
        self,
        *,
        connection: ConnectOptions | None = None,
        resources: ResourceSpec | None = None,
        **data: Any,
    ) -> None:
        if data.get("stateful") or data.get("optimize_data_file"):
            raise ValueError(
                "stateful Python and input file optimization are not supported"
            )
        super().__init__(**data)
        if connection is not None and connection.target not in (None, self.target):
            raise ValueError("connection target disagrees with target")
        self._connection = connection or ConnectOptions(target=self.target)
        self._resources = resources or ResourceSpec(network=False)

    def _session_machine(self, context: Any) -> _SessionMachine:
        session = context.session
        key = (session.app_name, session.user_id, session.id)
        with self._lock:
            if self._closed:
                raise RuntimeError("Smol code executor has been closed")
            if key not in self._sessions:
                self._sessions[key] = _SessionMachine()
            return self._sessions[key]

    def execute_code(
        self, invocation_context: Any, code_execution_input: CodeExecutionInput
    ) -> CodeExecutionResult:
        """Execute a code block, returning bounded stdout and stderr to ADK."""
        if code_execution_input.input_files:
            return CodeExecutionResult(
                stderr="Input files require a code executor with file optimization.",
                exit_code=1,
            )
        if len(code_execution_input.code.encode("utf-8")) > _MAX_CODE:
            return CodeExecutionResult(
                stderr="Code exceeds the 64 KiB VM command limit.", exit_code=1
            )
        slot = self._session_machine(invocation_context)
        with slot.lock:
            if self._closed or slot.closed:
                raise RuntimeError("Smol code executor session has been closed")
            if slot.machine is None:
                slot.machine = Machine.create(
                    MachineConfig(
                        image=self.image,
                        resources=self._resources,
                        ttl_seconds=(
                            self.cloud_ttl_seconds if self.target == "cloud" else None
                        ),
                    ),
                    self._connection,
                )
            stdout: list[str] = []
            stderr: list[str] = []
            remaining = _MAX_OUTPUT
            exit_code: int | None = None
            truncated = False
            with slot.machine.exec_stream(
                ["python3", "-c", code_execution_input.code],
                ExecOptions(timeout=self.timeout_seconds, workdir="/workspace"),
            ) as stream:
                for event in stream:
                    kind = event.get("kind")
                    if kind in ("stdout", "stderr"):
                        data = str(event.get("data", "")).encode("utf-8")
                        (stdout if kind == "stdout" else stderr).append(
                            data[:remaining].decode("utf-8", errors="ignore")
                        )
                        limit_reached = len(data) >= remaining
                        remaining = max(0, remaining - len(data))
                        if limit_reached:
                            truncated = True
                            stream.kill()
                            break
                    elif kind == "exit":
                        exit_code = int(event["exit_code"])
                    elif kind == "error":
                        stderr.append(str(event.get("message", "Execution failed")))
                        break
            output = "".join(stdout)
            error = "".join(stderr)
            if truncated:
                error += "\nOutput limit reached; output stream closed."
            elif exit_code == 0:
                error = ""
            elif exit_code is not None and not error:
                error = f"Code execution exited with status {exit_code}."
            elif exit_code is None and not error:
                error = "Code execution ended without an exit status."
            return CodeExecutionResult(stdout=output, stderr=error, exit_code=exit_code)

    def close_session(self, app_name: str, user_id: str, session_id: str) -> None:
        """Delete one session's VM when that ADK session finishes."""
        key = (app_name, user_id, session_id)
        with self._lock:
            slot = self._sessions.get(key)
            if slot is None:
                return
            with slot.lock:
                if slot.machine is not None:
                    try:
                        slot.machine.delete()
                    except Exception as exc:
                        if getattr(exc, "code", None) != "NOT_FOUND":
                            raise
                slot.closed = True
                del self._sessions[key]

    def close(self) -> None:
        """Delete session VMs, retaining failed deletions for a later retry."""
        with self._lock:
            if self._closed:
                return
            first_error: Exception | None = None
            for key, slot in list(self._sessions.items()):
                with slot.lock:
                    if slot.machine is not None:
                        try:
                            slot.machine.delete()
                        except Exception as exc:  # noqa: BLE001 - retry every SDK deletion failure
                            if getattr(exc, "code", None) != "NOT_FOUND":
                                if first_error is None:
                                    first_error = exc
                                continue
                    slot.closed = True
                    del self._sessions[key]
            if first_error is not None:
                raise first_error
            self._closed = True

    def __enter__(self) -> Self:
        if self._closed:
            raise RuntimeError("Smol code executor has been closed")
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()
