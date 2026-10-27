"""LlamaIndex function tools backed by a local or Cloud Smol Machines VM.

Install with ``pip install smolmachines[llama-index]``. Keep this object in a
``with`` block so VMs created for the agent are deleted when the run ends.
"""

from __future__ import annotations

import threading
from types import TracebackType
from typing import Optional

from .machine import Machine
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

_MAX_OUTPUT = 50 * 1024


class SmolLlamaIndexTools:
    """Give a LlamaIndex agent shell and Python tools in one isolated VM.

    ``machine`` is caller-owned and is never deleted by this adapter. A VM
    created by this adapter is deleted by ``close`` or on context exit.
    ``resources`` controls guest egress; without it, guest networking is off.
    The selected image must provide ``sh`` and ``python3``.
    """

    def __init__(
        self,
        *,
        image: str = "python:3.12-alpine",
        target: str = "local",
        connection: Optional[ConnectOptions] = None,
        resources: Optional[ResourceSpec] = None,
        machine: Optional[Machine] = None,
        timeout_seconds: int = 30,
    ) -> None:
        if target not in ("local", "cloud"):
            raise ValueError("target must be 'local' or 'cloud'")
        if connection is not None and connection.target not in (None, target):
            raise ValueError("connection target disagrees with target")
        if not 1 <= timeout_seconds <= 300:
            raise ValueError("timeout_seconds must be between 1 and 300")
        self._image = image
        self._target = target
        self._connection = connection or ConnectOptions(target=target)
        self._resources = resources or ResourceSpec(network=False)
        self._machine = machine
        self._owned = machine is None
        self._closed = False
        self._timeout = timeout_seconds
        self._lock = threading.RLock()

    def _get_machine(self) -> Machine:
        if self._closed:
            raise RuntimeError("Smol LlamaIndex tools have been closed")
        if self._machine is None:
            self._machine = Machine.create(
                MachineConfig(
                    image=self._image,
                    resources=self._resources,
                    ttl_seconds=3600 if self._target == "cloud" else None,
                ),
                self._connection,
            )
        return self._machine

    def _execute(self, command: list[str]) -> str:
        with self._lock:
            machine = self._get_machine()
            stdout: list[str] = []
            stderr: list[str] = []
            remaining = _MAX_OUTPUT
            exit_code: Optional[int] = None
            truncated = False
            with machine.exec_stream(
                command, ExecOptions(timeout=self._timeout, workdir="/workspace")
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
                        raise RuntimeError(
                            f"VM command failed: {event.get('message', 'unknown error')}"
                        )
            parts = [f"stdout:\n{''.join(stdout)}", f"stderr:\n{''.join(stderr)}"]
            if truncated:
                parts.append("Output limit reached; output stream closed.")
            else:
                parts.append(
                    f"exit_code: {exit_code if exit_code is not None else 'unknown'}"
                )
            return "\n".join(parts)

    def run_command(self, command: str) -> str:
        """Run a shell command inside the VM and return bounded output and exit code."""
        if len(command) > 32 * 1024:
            raise ValueError("command is too long")
        return self._execute(["sh", "-c", command])

    def run_python(self, code: str) -> str:
        """Run Python code inside the VM; files in /workspace persist across calls."""
        if len(code) > 32 * 1024:
            raise ValueError("code is too long")
        return self._execute(["python3", "-c", code])

    def tools(self) -> list:
        """Return LlamaIndex FunctionTools for a FunctionAgent or AgentWorkflow."""
        try:
            from llama_index.core.tools import FunctionTool
        except ImportError as exc:
            raise ImportError(
                "Install smolmachines[llama-index] to use the LlamaIndex tools"
            ) from exc
        return [
            FunctionTool.from_defaults(fn=self.run_command, name="smol_run_command"),
            FunctionTool.from_defaults(fn=self.run_python, name="smol_run_python"),
        ]

    def close(self) -> None:
        """Delete the owned VM. A failed deletion can be retried with close()."""
        with self._lock:
            if self._closed:
                return
            if self._owned and self._machine is not None:
                self._machine.delete()
                self._machine = None
            self._closed = True

    def __enter__(self) -> "SmolLlamaIndexTools":
        if self._closed:
            raise RuntimeError("Smol LlamaIndex tools have been closed")
        return self

    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[TracebackType],
    ) -> None:
        self.close()
