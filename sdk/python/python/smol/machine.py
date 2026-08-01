"""The public ``Machine`` API — one interface over local (embedded) or cloud.

Mirrors the Node SDK's ``machine.ts`` (sync, since the embedded engine blocks)::

    from smol import Machine, MachineConfig, ResourceSpec

    with Machine.create(MachineConfig(resources=ResourceSpec(cpus=2, memory_mb=1024))) as m:
        res = m.run("python:3.12", ["python", "-c", "print(2 ** 10)"])
        res.assert_success()
        print(res.stdout)  # 1024
"""

from __future__ import annotations

import uuid
from typing import Any, Optional

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

        On the cloud target, this does not return until the machine is ready for
        work: the guest agent is reachable and any published port is accepting
        connections. Cloud state ``"started"`` alone means only that the VM
        process launched.

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

          This does not wait for readiness; call :meth:`wait_until_ready`
          before ``exec``, using a connect endpoint, or expecting the workload
          to respond.

        :param machine_id: local machine name, or cloud machine id (``mach-…``).
        :param conn: backend selection (local by default; cloud via
            ``ConnectOptions(target='cloud', api_key=…)`` or ``SMOL_CLOUD_TOKEN``).
        """
        return cls(connect_transport(machine_id, conn))

    @property
    def name(self) -> str:
        """The machine's name / identifier."""
        return self._t.name

    @property
    def id(self) -> str:
        """The machine's id — the cloud ``mach-…`` id (or the name on local). Handy
        right after :meth:`create`/:meth:`fork` so you don't have to list the
        tenant to find the machine you just made."""
        return self._t.machine_id

    def state(self) -> str:
        """Current lifecycle state. On cloud, ``"started"`` means only that the
        VM process launched; use :meth:`ready` or :meth:`wait_until_ready`
        before doing work."""
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

    def start(self) -> None:
        """Start (resume) a stopped machine, waiting until its agent is ready. The
        counterpart to :meth:`stop` — a stopped machine keeps its disk state, so
        ``start()`` brings it back with that state intact (cheap pause/resume)."""
        self._t.start()

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

    def fork_batch(
        self,
        count: Optional[int] = None,
        *,
        names: Optional[list[str]] = None,
        name_prefix: Optional[str] = None,
        ports: Optional[list[PortSpec]] = None,
    ) -> "list[Machine]":
        """Fork this forkable machine into MANY clones in one call — the RL
        fan-out primitive (GRPO group sampling / eval-task fan-out). Each clone is
        a live-RAM CoW fork off this golden, like :meth:`fork`. On the cloud target
        the batch is transactional (all-or-nothing: if any clone fails, the whole
        batch is rolled back), so you get all N branches or none. Requires this
        machine to be ``forkable``.

        Provide either ``count`` (clones auto-named ``{name_prefix}-{n}``) or
        explicit ``names``.

        :param count: number of clones to fork (ignored when ``names`` is given).
        :param names: explicit clone names; its length is the batch size.
        :param name_prefix: prefix for auto-named clones (default: the golden's
            name on the cloud target, else ``"fork"``).
        :param ports: optional pinned inbound port forwards applied to every clone.
        :returns: the clones, in request order.
        """
        return [
            Machine(t)
            for t in self._t.fork_batch(
                count, names=names, name_prefix=name_prefix, ports=ports
            )
        ]

    def assign(
        self,
        lease_id: str,
        *,
        task: object = None,
        files: Optional[dict[str, bytes]] = None,
        secrets: Optional[dict[str, str]] = None,
        ports: Optional[list[PortSpec]] = None,
        ttl_secs: Optional[int] = None,
        heartbeat_secs: Optional[int] = None,
    ) -> "Episode":
        """Assign an RL episode: fork + provision a clone of this forkable golden
        under an idempotent lease, and get it back only once fully installed —
        with lifecycle control (:meth:`Episode.heartbeat`, :meth:`Episode.complete`).
        The scheduler's "give me one clean worker for this task, exactly once"
        call. **Cloud target only** (leases live in the control plane).

        Idempotent on ``lease_id``: a retried ``assign`` with the same key returns
        the SAME episode instead of forking a second one. The ``task`` payload is
        staged into the clone at ``/run/smol-task.json``, ``secrets`` at
        ``/run/smol-secrets.json``, and ``files`` at their given paths — all before
        the episode is handed back, so a returned :class:`Episode` is always fully
        provisioned. Use it as a context manager to guarantee teardown::

            with golden.assign("task-42", task={"seed": 7}) as ep:
                ep.exec(["python", "rollout.py"])   # clone auto-completed on exit

        :param lease_id: caller-chosen idempotency key.
        :param task: JSON-serializable task payload.
        :param files: ``{guest_path: bytes}`` to stage into the clone.
        :param secrets: ``{name: value}`` staged as a JSON file, never persisted.
        :param ports: optional pinned inbound port forwards.
        :param ttl_secs: lease time-to-live; the clone is reclaimed past it.
        :param heartbeat_secs: expected heartbeat cadence; reclaimed if missed.
        :returns: an :class:`Episode` handle.
        """
        clone_t, lid, owner_token = self._t.assign(
            lease_id,
            task=task,
            files=files,
            secrets=secrets,
            ports=ports,
            ttl_secs=ttl_secs,
            heartbeat_secs=heartbeat_secs,
        )
        return Episode(Machine(clone_t), lid, owner_token, self._t)

    def reward_fork(
        self,
        command: list[str],
        opts: Optional[ExecOptions] = None,
        name: Optional[str] = None,
    ) -> ExecResult:
        """Score this machine's state WITHOUT mutating it.

        Forks an ephemeral clone, runs the grader ``command`` in the clone, then
        destroys it — so grading never touches this machine. The returned
        :class:`ExecResult` is your reward signal (``.exit_code == 0`` for
        pass/fail, ``.stdout`` for a parsed score). This is the RL "reward judging
        without
        side effects" primitive (grade the end of a rollout, or run a verifier)
        as one call; the throwaway clone is always cleaned up, even if the grader
        raises. Requires this machine to be ``forkable`` (like :meth:`fork`).

        :param command: the grader/verifier argv to run in the clone.
        :param opts: exec options (env, cwd, timeout, …).
        :param name: optional clone name (default: a generated unique name).
        :returns: the grader's :class:`ExecResult`.
        """
        clone = self.fork(name or f"{self.name}-rf-{uuid.uuid4().hex[:8]}")
        try:
            return clone.exec(command, opts)
        finally:
            # Best-effort teardown — never mask the grader's result/error.
            try:
                clone.delete()
            except Exception:
                pass

    # -- context manager: auto-delete on exit (ergonomic for ephemeral use) --
    def __enter__(self) -> "Machine":
        return self

    def __exit__(self, *exc: object) -> None:
        self.delete()


class Episode:
    """A leased RL episode: a freshly-provisioned clone plus its lease. Created by
    :meth:`Machine.assign`. Use it as a context manager so the clone is always torn
    down when the block exits (even on error) — the RL "one clean worker per task,
    guaranteed reclaimed" pattern. Cloud target only.
    """

    def __init__(
        self,
        machine: Machine,
        lease_id: str,
        owner_token: str,
        lease_transport: "Transport",
    ) -> None:
        self._machine = machine
        self.lease_id = lease_id
        self._owner_token = owner_token
        self._lease_t = lease_transport
        self._completed = False

    @property
    def machine(self) -> Machine:
        """The episode's provisioned clone."""
        return self._machine

    def exec(self, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        """Run a command in the episode's machine (shortcut for ``.machine.exec``)."""
        return self._machine.exec(command, opts)

    def heartbeat(self) -> None:
        """Keep the lease alive. Call within the ``heartbeat_secs`` cadence you
        requested, or the episode is reclaimed as ``heartbeat_lost``."""
        self._lease_t.heartbeat_lease(self.lease_id, self._owner_token)

    def complete(
        self,
        reason: str = "done",
        *,
        score: Optional[float] = None,
        result: Any = None,
    ) -> None:
        """Finish the episode with a typed termination reason (e.g. ``done``,
        ``agent_failed``, ``infra_failed``, ``cancelled``) and tear its clone down.
        Optionally record a ``score`` (the per-task reward / pass-rate) and an
        arbitrary JSON ``result`` — the "did it improve?" number, read back later
        via :meth:`status`. Idempotent — a second call is a no-op."""
        if self._completed:
            return
        self._lease_t.complete_lease(
            self.lease_id, self._owner_token, reason, score=score, result=result
        )
        self._completed = True

    def status(self) -> "dict[str, Any]":
        """Read this lease's current status and, once completed, its outcome
        (``state``, ``reason``, ``score``, ``result``) — how a trainer collects the
        per-task score after the episode ends."""
        return self._lease_t.get_lease(self.lease_id)

    def __enter__(self) -> "Episode":
        return self

    def __exit__(self, exc_type: object, *_: object) -> None:
        # Always release the episode; a clean exit is `done`, an exception is
        # `agent_failed` so a trainer can tell task failure from infra failure.
        try:
            self.complete("done" if exc_type is None else "agent_failed")
        except Exception:
            pass
