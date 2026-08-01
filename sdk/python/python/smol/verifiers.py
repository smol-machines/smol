"""Use smol as a `verifiers` runtime — the RL rollout execution backend.

`verifiers` (Prime Intellect) decomposes an RL environment into a *taskset*, a
*harness*, and a *runtime*: the runtime is *where* a rollout's code executes, behind
a small ``start``/``run``/``read``/``write``/``teardown`` contract, with the
framework guaranteeing each rollout gets an isolated runtime. ``SmolRuntime`` is an
implementation of that contract backed by smol microVMs.

Point a verifiers env at it in place of the Docker/Prime/Modal runtimes::

    from smol.verifiers import SmolRuntime, SmolConfig

    runtime = SmolRuntime(SmolConfig(image="python:3.12-slim"))
    # ...verifiers drives runtime.start() / run() / read() / write() / teardown()

**Fork mode (the smol edge).** Point ``golden`` at a *running, forkable* smol golden
and every rollout is a copy-on-write clone of it — sub-second, dense (clones share
the golden's RAM/disk), and starting from the exact same prepared state::

    SmolConfig(golden="mach-…")   # each rollout = one fork of the golden

Requires ``verifiers`` (``pip install verifiers``) and the ``smolmachines`` SDK.
Only the pure config->resources mapping is importable without verifiers; the
runtime class is defined only when verifiers is present.
"""

from __future__ import annotations

import uuid
from typing import Any, ClassVar, Optional

from .async_machine import AsyncMachine
from .types import ConnectOptions, ExecOptions, MachineConfig, ResourceSpec

try:
    from pydantic_config import BaseConfig
    from verifiers.v1.runtimes.base import BaseRuntimeInfo, ProgramResult, Runtime

    _HAS_VERIFIERS = True
except ImportError:  # pragma: no cover - exercised only where verifiers is absent
    _HAS_VERIFIERS = False


def _resources(
    cpu: float,
    memory_gb: float,
    disk_gb: float,
    network_access: bool,
    allow_hosts: Optional[list[str]] = None,
    allow_cidrs: Optional[list[str]] = None,
) -> ResourceSpec:
    """Map a runtime config to a smol :class:`ResourceSpec`. verifiers uses a single
    ``network_access`` boolean; smol additionally supports scoping egress to specific
    hosts/CIDRs, so ``allow_hosts``/``allow_cidrs`` (a smol extra) enable networking
    and restrict it when set."""
    scoped = bool(allow_hosts or allow_cidrs)
    return ResourceSpec(
        cpus=int(cpu),
        memory_mb=int(memory_gb * 1024),
        storage_gb=int(disk_gb),
        network=True if scoped else network_access,
        allow_hosts=allow_hosts,
        allow_cidrs=allow_cidrs,
    )


if _HAS_VERIFIERS:

    from typing import Literal

    class SmolConfig(BaseConfig):
        """Configuration for a :class:`SmolRuntime` (mirrors the other verifiers
        runtime configs; units follow ``TaskData.resources``)."""

        type: Literal["smol"] = "smol"
        image: str = "python:3.11-slim"
        workdir: Optional[str] = None
        network_access: bool = True
        cpu: float = 1.0
        """CPU cores."""
        memory: float = 2.0
        """Memory in GB."""
        disk: float = 5.0
        """Disk in GB."""
        forkable: bool = False
        golden: Optional[str] = None
        """A running, forkable smol golden to fork per rollout (fork mode). When set,
        each rollout is a CoW clone of it and image/cpu/memory are inherited."""
        target: Literal["local", "cloud"] = "cloud"
        allow_hosts: Optional[list[str]] = None
        """smol extra: scope egress to these hostnames (and subdomains)."""
        allow_cidrs: Optional[list[str]] = None
        """smol extra: scope egress to these CIDR ranges."""

    class SmolRuntimeInfo(SmolConfig, BaseRuntimeInfo):
        pass

    class SmolRuntime(Runtime):
        """A verifiers runtime that executes each rollout in an isolated smol
        microVM — a fresh machine, or a CoW fork of a golden when ``golden`` is set.
        """

        is_local: ClassVar[bool] = False

        def __init__(self, config: "SmolConfig | None" = None, name: str | None = None) -> None:
            super().__init__(name)
            self.config = config or SmolConfig()
            self.info = SmolRuntimeInfo(**self.config.model_dump())
            self._connect = ConnectOptions(target=self.config.target)
            self._machine: AsyncMachine | None = None

        async def start(self) -> None:
            if self.config.golden:
                # Fork mode: one CoW clone of the prepared golden per rollout.
                golden = await AsyncMachine.connect(self.config.golden, self._connect)
                self._machine = await golden.fork(f"vf-{uuid.uuid4().hex[:12]}")
            else:
                self._machine = await AsyncMachine.create(
                    MachineConfig(
                        image=self.config.image,
                        workdir=self.config.workdir,
                        forkable=self.config.forkable,
                        resources=_resources(
                            self.config.cpu,
                            self.config.memory,
                            self.config.disk,
                            self.config.network_access,
                            self.config.allow_hosts,
                            self.config.allow_cidrs,
                        ),
                    ),
                    self._connect,
                )
            self.info.id = self._machine.id

        async def run(self, argv: list[str], env: dict[str, str]) -> "ProgramResult":
            assert self._machine is not None, "runtime not started"
            r = await self._machine.exec(
                list(argv),
                ExecOptions(env=dict(env or {}), workdir=self.config.workdir),
            )
            return ProgramResult(exit_code=r.exit_code, stdout=r.stdout, stderr=r.stderr)

        async def read(self, path: str) -> bytes:
            assert self._machine is not None, "runtime not started"
            return await self._machine.read_file(path)

        async def write(self, path: str, data: bytes) -> None:
            assert self._machine is not None, "runtime not started"
            await self._machine.write_file(path, data)

        async def teardown(self) -> None:
            machine, self._machine = self._machine, None
            if machine is not None:
                try:
                    await machine.delete()
                except Exception:  # noqa: BLE001 - teardown is best-effort
                    pass

        def cleanup(self) -> None:
            # Sync, idempotent — the async teardown does the real work.
            self._machine = None

        async def expose(self, port: int) -> str | None:
            # smol publishes ports at create time; return the machine's URL if it has
            # a published/public port, else None.
            if self._machine is None:
                return None
            return await self._machine.url()

else:  # verifiers not installed — a helpful stub so imports don't hard-fail.

    class SmolRuntime:  # type: ignore[no-redef]
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            raise ImportError(
                "SmolRuntime requires the `verifiers` package: pip install verifiers"
            )
