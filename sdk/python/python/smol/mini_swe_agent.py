"""mini-swe-agent environment backed by a smol machine.

mini-swe-agent loads an environment by dotted path, so it needs no change::

    environment:
      environment_class: smol.mini_swe_agent.SmolEnvironment
      image: python:3.11

``SmolEnvironment`` follows mini-swe-agent's ``DockerEnvironment``: one machine
per environment, created in ``__init__`` and deleted by ``cleanup()``; each
action runs as ``interpreter + [command]`` with stderr merged into stdout, and
a timeout or SDK error comes back as ``returncode == -1`` with
``exception_info`` set. Set ``rosetta: true`` to run x86_64 images, such as
SWE-bench's, on Apple Silicon.

Requires ``pip install smolmachines[mini-swe-agent]``.
"""

from __future__ import annotations

import logging
import os
import platform
import time
import uuid
from typing import Any

from minisweagent.exceptions import Submitted
from minisweagent.utils.serialize import recursive_merge
from pydantic import BaseModel

from .errors import SmolError
from .machine import Machine
from .types import ExecOptions, MachineConfig, ResourceSpec

# `timeout(1)`'s status, which the engine also returns for a command it kills.
_TIMEOUT_EXIT = 124


class SmolEnvironmentConfig(BaseModel):
    image: str
    cwd: str = "/"
    """Working directory in which to execute commands."""
    env: dict[str, str] = {}
    """Environment variables to set in the machine."""
    forward_env: list[str] = []
    """Host environment variables to forward when set; `env` wins on conflict."""
    timeout: int = 30
    """Timeout in seconds for executing a command."""
    pull_timeout: int = 120
    """Seconds the machine may take to pull its image and boot."""
    interpreter: list[str] = ["bash", "-lc"]
    """Interpreter the command is appended to."""
    cpus: int | None = None
    """vCPUs; the engine default when unset."""
    memory_mb: int | None = None
    """Memory in MB; the engine default when unset."""
    network: bool = True
    """Outbound network, which pulling the image needs."""
    rosetta: bool = False
    """Run x86_64 images through Rosetta 2 on Apple Silicon; needs `network`."""


class SmolEnvironment:
    def __init__(
        self,
        *,
        config_class: type = SmolEnvironmentConfig,
        logger: logging.Logger | None = None,
        **kwargs: Any,
    ) -> None:
        self.logger = logger or logging.getLogger("minisweagent.environment")
        self.config = config_class(**kwargs)
        self.machine = Machine.create(
            MachineConfig(
                name=f"minisweagent-{uuid.uuid4().hex[:8]}",
                image=self.config.image,
                resources=ResourceSpec(
                    cpus=self.config.cpus,
                    memory_mb=self.config.memory_mb,
                    network=self.config.network,
                    rosetta=self.config.rosetta,
                ),
                ready_timeout_seconds=self.config.pull_timeout,
            )
        )
        self.logger.info(f"Started smol machine {self.machine.name}")

    def get_template_vars(self, **kwargs: Any) -> dict[str, Any]:
        return recursive_merge(self.config.model_dump(), platform.uname()._asdict(), kwargs)

    def serialize(self) -> dict:
        return {
            "info": {
                "config": {
                    "environment": self.config.model_dump(mode="json"),
                    "environment_type": f"{self.__class__.__module__}.{self.__class__.__name__}",
                }
            }
        }

    def execute(
        self, action: dict, cwd: str = "", *, timeout: int | None = None
    ) -> dict[str, Any]:
        command = action.get("command", "")
        assert self.machine is not None, "Machine not started"
        timeout = timeout or self.config.timeout
        env = {k: v for k in self.config.forward_env if (v := os.getenv(k)) is not None}
        env.update(self.config.env)
        argv = ["/bin/sh", "-c", 'exec "$@" 2>&1', "sh", *self.config.interpreter, command]
        started = time.monotonic()
        try:
            result = self.machine.exec(
                argv, ExecOptions(env=env or None, workdir=cwd or self.config.cwd, timeout=timeout)
            )
        except SmolError as e:
            output = {
                "output": "",
                "returncode": -1,
                "exception_info": f"An error occurred while executing the command: {e}",
                "extra": {"exception_type": type(e).__name__, "exception": str(e)},
            }
        else:
            if result.exit_code == _TIMEOUT_EXIT and time.monotonic() - started >= timeout:
                output = {
                    "output": result.stdout,
                    "returncode": -1,
                    "exception_info": "An error occurred while executing the command: "
                    f"Command '{command}' timed out after {timeout} seconds",
                    "extra": {
                        "exception_type": "TimeoutExpired",
                        "exception": result.stderr.strip(),
                    },
                }
            else:
                output = {
                    "output": result.stdout + result.stderr,
                    "returncode": result.exit_code,
                    "exception_info": "",
                }
        self._check_finished(output)
        return output

    def _check_finished(self, output: dict) -> None:
        lines = output.get("output", "").lstrip().splitlines(keepends=True)
        if (
            lines
            and lines[0].strip() == "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT"
            and output["returncode"] == 0
        ):
            submission = "".join(lines[1:])
            raise Submitted(
                {
                    "role": "exit",
                    "content": submission,
                    "extra": {"exit_status": "Submitted", "submission": submission},
                }
            )

    def cleanup(self) -> None:
        machine, self.machine = getattr(self, "machine", None), None
        if machine is not None:
            try:
                machine.delete()
            except SmolError as e:
                self.logger.warning(f"Deleting smol machine {machine.name} failed: {e}")

    def __del__(self) -> None:
        self.cleanup()
