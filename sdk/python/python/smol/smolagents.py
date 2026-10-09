"""Smol Machines executor for smolagents ``CodeAgent(executor=...)``.

Install ``smolmachines[smolagents]`` and create a SmolExecutor. Code and Python
state run inside a microVM; final answers use safe serialization by default.
"""

from __future__ import annotations

import contextlib
import json
import shlex
import uuid
from pathlib import Path

from smolagents.local_python_executor import CodeOutput
from smolagents.remote_executors import RemotePythonExecutor
from smolagents.tools import get_tools_definition_code
from smolagents.utils import AgentError

from .machine import Machine
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

_GUEST_ROOT = "/tmp/smolagents-executor"
_CLIENT = """import json,socket,sys
with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as sock:
 sock.connect(sys.argv[1])
 payload=open(sys.argv[2],'rb').read()
 sock.sendall(len(payload).to_bytes(8,'big')+payload)
 header=bytearray()
 while len(header)<8:
  part=sock.recv(8-len(header))
  if not part: raise ConnectionError('Worker ended before response header')
  header.extend(part)
 size=int.from_bytes(header,'big')
 data=bytearray()
 while len(data)<size:
  part=sock.recv(min(65536,size-len(data)))
  if not part: raise ConnectionError('Worker ended without a response')
  data.extend(part)
 sys.stdout.write(data.decode('utf-8'))
"""


class SmolExecutor(RemotePythonExecutor):
    """Execute smolagents Python actions in a persistent local or cloud microVM.

    The image must provide ``python3`` and ``pip``. A guest network policy is
    required only if tools or additional imports must be installed at runtime;
    an image with those dependencies preinstalled can keep guest egress closed.
    ``machine`` accepts a caller-owned VM; the executor only deletes VMs it creates.
    """

    def __init__(
        self,
        additional_imports: list[str] | None = None,
        logger=None,
        *,
        image: str = "python:3.12-slim",
        target: str = "local",
        connection: ConnectOptions | None = None,
        resources: ResourceSpec | None = None,
        machine: Machine | None = None,
        timeout_seconds: int = 60,
        allow_pickle: bool = False,
    ) -> None:
        if timeout_seconds < 1 or timeout_seconds > 3600:
            raise ValueError("timeout_seconds must be between 1 and 3600")
        if logger is None:
            from smolagents.monitoring import AgentLogger

            logger = AgentLogger()
        if target not in ("local", "cloud"):
            raise ValueError("target must be 'local' or 'cloud'")
        super().__init__(additional_imports or [], logger, allow_pickle=allow_pickle)
        self.timeout_seconds = timeout_seconds
        self._owned = machine is None
        self._machine: Machine | None = None
        self._worker_started = False
        self._directory = f"{_GUEST_ROOT}/{uuid.uuid4().hex}"
        self._socket = f"{self._directory}/worker.sock"
        try:
            self._machine = machine or Machine.create(
                MachineConfig(
                    image=image,
                    resources=resources or ResourceSpec(network=False),
                    wait_for_ports=False,
                ),
                connection or ConnectOptions(target=target),
            )
            self._machine.exec(
                ["mkdir", "-m", "700", "-p", self._directory]
            ).assert_success()
            self._machine.write_file(
                f"{self._directory}/worker.py",
                Path(__file__).with_name("_smolagents_worker.py").read_bytes(),
            )
            launcher = (
                f"python3 -u {shlex.quote(self._directory + '/worker.py')} {shlex.quote(self._socket)} "
                f">{shlex.quote(self._directory + '/worker.log')} 2>&1 </dev/null &"
            )
            self._machine.exec(["/bin/sh", "-c", launcher]).assert_success()
            for _ in range(40):
                probe = self._machine.exec(
                    ["/bin/sh", "-c", f"test -S {shlex.quote(self._socket)}"]
                )
                if probe.exit_code == 0:
                    break
                import time

                time.sleep(0.1)
            else:
                logs = self._machine.exec(["cat", f"{self._directory}/worker.log"])
                raise RuntimeError(
                    f"Smol Python worker failed to start: {logs.stdout} {logs.stderr}"
                )
            self._worker_started = True
            self.installed_packages = self.install_packages(self.additional_imports)
        except BaseException:
            with contextlib.suppress(Exception):
                self.cleanup()
            raise

    def send_tools(self, tools: dict) -> None:
        # The patched final_answer tool is self-contained. Its source analysis
        # otherwise lists optional image packages even for plain string answers.
        if "final_answer" in tools:
            final_answer = tools["final_answer"]
            if not getattr(final_answer, "_smol_final_answer_patched", False):
                self._patch_final_answer_with_exception(final_answer)
                final_answer._smol_final_answer_patched = True
        packages = {
            ("pillow" if pkg == "PIL" else pkg)
            for name, tool in tools.items()
            if name != "final_answer"
            for pkg in tool.to_dict()["requirements"]
            if pkg != "smolagents"
        }
        packages.difference_update(self.installed_packages)
        if packages:
            self.installed_packages += self.install_packages(sorted(packages))
        code = get_tools_definition_code(tools)
        # The patched final_answer source references image/array types only in
        # lazy optional branches; upstream also emits eager imports for them.
        if "pillow" not in packages and "pillow" not in self.installed_packages:
            code = code.replace("\nimport PIL\n", "\n")
        if "numpy" not in packages and "numpy" not in self.installed_packages:
            code = code.replace("\nimport numpy\n", "\n")
        if code:
            self.logger.log(self.run_code_raise_errors(code).logs)

    def run_code_raise_errors(self, code: str) -> CodeOutput:
        if self._machine is None:
            raise RuntimeError("Smol executor has been closed")
        request = f"{self._directory}/{uuid.uuid4().hex}.json"
        self._machine.write_file(
            request, json.dumps({"code": code, "timeout": self.timeout_seconds})
        )
        try:
            response = self._machine.exec(
                ["python3", "-c", _CLIENT, self._socket, request],
                ExecOptions(timeout=self.timeout_seconds + 15),
            )
            if response.exit_code != 0:
                raise RuntimeError(
                    f"Smol action transport failed: {response.stderr or response.stdout}"
                )
            data = json.loads(response.stdout)
            if error := data.get("error"):
                raise AgentError(f"{data.get('logs', '')}\n{error}", self.logger)
            if data.get("final"):
                return CodeOutput(
                    output=self._deserialize_final_answer(
                        data["result"], self.allow_pickle
                    ),
                    logs=data["logs"],
                    is_final_answer=True,
                )
            return CodeOutput(
                output=data["result"], logs=data["logs"], is_final_answer=False
            )
        finally:
            with contextlib.suppress(Exception):
                self._machine.exec(["rm", "-f", request])

    def install_packages(self, additional_imports: list[str]) -> list[str]:
        if not additional_imports:
            return []
        if self._machine is None:
            raise RuntimeError("Smol executor has been closed")
        for package in additional_imports:
            if not package or package.startswith("-"):
                raise ValueError(f"Invalid package requirement: {package!r}")
        response = self._machine.exec(
            ["python3", "-m", "pip", "install", "--", *additional_imports],
            ExecOptions(timeout=300),
        )
        if response.exit_code != 0:
            raise RuntimeError(
                f"Smol dependency installation failed: {response.stderr or response.stdout}"
            )
        return additional_imports

    def cleanup(self) -> None:
        machine, self._machine = self._machine, None
        if machine is None:
            return
        if self._owned:
            machine.delete()
        else:
            if self._worker_started:
                request = f"{self._directory}/shutdown.json"
                with contextlib.suppress(Exception):
                    machine.write_file(request, '{"shutdown":true}')
                    machine.exec(
                        ["python3", "-c", _CLIENT, self._socket, request],
                        ExecOptions(timeout=5),
                    )
            with contextlib.suppress(Exception):
                machine.exec(["rm", "-rf", self._directory])
