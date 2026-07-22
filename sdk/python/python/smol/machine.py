"""The public ``Machine`` API — one interface over local (embedded) or cloud.

Mirrors the Node SDK's ``machine.ts`` (sync, since the embedded engine blocks)::

    from smol import Machine, MachineConfig, ResourceSpec

    with Machine.create(MachineConfig(resources=ResourceSpec(cpus=2, memory_mb=1024))) as m:
        res = m.run("python:3.12", ["python", "-c", "print(2 ** 10)"])
        res.assert_success()
        print(res.stdout)  # 1024
"""

from __future__ import annotations

from typing import Optional

from .transport import Transport, connect_transport, make_transport
from .types import (
    ConnectOptions,
    ExecOptions,
    ExecResult,
    ImageInfo,
    MachineConfig,
    PortEndpoint,
    PortSpec,
)

__all__ = ["Machine"]


class Machine:
    """A running microVM sandbox. Create with :meth:`create`; clean up with
    :meth:`delete` (or use it as a context manager)."""

    def __init__(self, transport: Transport) -> None:
        self._t = transport

    @classmethod
    def create(
        cls,
        config: Optional[MachineConfig] = None,
        conn: Optional[ConnectOptions] = None,
    ) -> "Machine":
        """Create and start a machine.

        :param config: machine configuration (a name is generated if omitted;
            ``image`` is required for the cloud target, optional for local).
        :param conn: backend selection (local embedded by default).
        """
        return cls(make_transport(config or MachineConfig(), conn))

    @classmethod
    def connect(
        cls,
        machine_id: str,
        conn: Optional[ConnectOptions] = None,
    ) -> "Machine":
        """Attach to an EXISTING machine without creating a new one — to drive a
        machine made elsewhere (another process, the console, the REST API).

        * local (default): re-opens a persisted machine by NAME, starting it if
          stopped — pairs with ``Machine.create(MachineConfig(name=…),
          persistent=True)``.
        * cloud: looks up the machine by id; raises if it doesn't exist.

        :param machine_id: local machine name, or cloud machine id (``mach-…``).
        :param conn: backend selection (local by default; cloud via
            ``ConnectOptions(target='cloud', api_key=…)`` or ``SMOL_CLOUD_TOKEN``).
        """
        return cls(connect_transport(machine_id, conn))

    @property
    def name(self) -> str:
        """The machine's name / identifier."""
        return self._t.name

    def state(self) -> str:
        """Current state, e.g. ``"running"`` / ``"stopped"``."""
        return self._t.state()

    def ready(self) -> bool:
        """Whether the machine is READY to do work. :meth:`state` becoming
        ``"started"`` means only that the VM process launched — the guest is
        still booting and is NOT yet usable. ``ready`` becomes true once the
        in-VM agent is reachable (an ``exec``/``connect`` will succeed) and any
        published port accepts connections. Gate on this, not ``state``, before
        driving the machine. (cloud; local reports ready once running.)"""
        return self._t.ready()

    def ready_at(self) -> Optional[str]:
        """When the machine first became ready (RFC3339), or ``None`` if not yet."""
        return self._t.ready_at()

    def wait_until_ready(self, timeout_s: float = 120.0, interval_s: float = 1.0) -> None:
        """Block until the machine is ``ready`` (or raise on a failed/stopped
        state or timeout). :meth:`create` already waits for readiness, so this is
        for machines attached via :meth:`connect`, or to re-assert readiness."""
        self._t.wait_until_ready(timeout_s, interval_s)

    def endpoint(self, port: int, path: Optional[str] = None) -> PortEndpoint:
        """An authenticated endpoint (URL + headers) to reach a PUBLISHED guest
        port through the control plane's connect bridge — no Cloudflare/
        localhost.run tunnel, no public exposure, no egress allow-list. Have the
        in-VM worker LISTEN on the port and connect *inbound*: plug ``ws_url``
        into a WebSocket client (passing ``headers``) or ``http_url`` into an
        HTTP client. The machine must publish the port
        (``ports=[PortSpec(...)]`` at create). (cloud)"""
        return self._t.endpoint(port, path)

    def request(
        self,
        port: int,
        path: Optional[str] = None,
        method: str = "GET",
        data: Optional[bytes] = None,
        timeout_s: float = 30.0,
    ) -> bytes:
        """Convenience: an authenticated HTTP request to a published guest port
        via the connect bridge; returns the raw response body bytes. (cloud)"""
        return self._t.request(port, path, method, data, timeout_s)

    def url(self) -> Optional[str]:
        """Public ingress URL for the machine's first published port (cloud).

        ``None`` until the machine is started with an allocated host port, for
        machines with no published port, or on the local target (no public
        ingress). Reach the deployed app over HTTPS at the returned URL.
        """
        return self._t.url()

    def exec(self, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        """Execute a command directly in the machine."""
        return self._t.exec(command, opts)

    def run(self, image: str, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        """Pull an image (if needed) and run a command in a container of it. (local)"""
        return self._t.run(image, command, opts)

    def exec_stream(self, command: list[str], opts: Optional[ExecOptions] = None):
        """Execute a command and stream its output LIVE as it is produced (local).

        Yields event dicts:
        ``{"kind": "stdout"|"stderr", "data": str}``,
        ``{"kind": "exit", "exit_code": int}``, or
        ``{"kind": "error", "message": str}``.
        """
        return self._t.exec_stream(command, opts)

    def read_file(self, path: str) -> bytes:
        """Read a file from the machine."""
        return self._t.read_file(path)

    def write_file(self, path: str, data: bytes | str, mode: Optional[int] = None) -> None:
        """Write a file into the machine."""
        payload = data.encode() if isinstance(data, str) else data
        self._t.write_file(path, payload, mode)

    def pull_image(self, image: str) -> ImageInfo:
        """Pull an OCI image into the machine's storage. (local)"""
        return self._t.pull_image(image)

    def list_images(self) -> list[ImageInfo]:
        """List cached OCI images. (local)"""
        return self._t.list_images()

    def stop(self) -> None:
        """Stop the machine."""
        self._t.stop()

    def delete(self) -> None:
        """Stop the machine and delete its storage."""
        self._t.delete()

    def fork(self, name: str, ports: Optional[list[PortSpec]] = None) -> "Machine":
        """Fork this running, forkable machine into a new clone via copy-on-write
        live RAM + disks (cloud target). The clone inherits the golden's warm
        in-memory state and runs on the same node; forks are fast (~tens of ms)
        and repeatable from one golden — the basis for RL rollout branching and
        instant episode reset. The golden must have been created with
        ``MachineConfig(forkable=True)``.

        :param name: name for the new clone machine.
        :param ports: optional pinned inbound port forwards for the clone (each
            ``PortSpec(host, guest)``); by default the node allocates fresh host
            ports so clones don't collide.
        :returns: a :class:`Machine` handle to the running clone.
        """
        return Machine(self._t.fork(name, ports))

    # -- context manager: auto-delete on exit (ergonomic for ephemeral use) --
    def __enter__(self) -> "Machine":
        return self

    def __exit__(self, *exc: object) -> None:
        self.delete()
