"""Async wrapper over :class:`Machine` — the same API, but every call that does
I/O is awaitable and runs off the event loop, so nothing blocks.

The sync :class:`Machine` (and its transports) is the single source of truth for
behaviour; ``AsyncMachine`` just offloads each blocking call to a worker thread
via :func:`asyncio.to_thread`. That keeps one implementation of the wire
protocol, readiness gating, and capability rules, while letting callers drive
many machines concurrently::

    import asyncio
    from smol import AsyncMachine, MachineConfig, ConnectOptions, PortSpec

    async def main():
        # Launch a fleet of disposable workers concurrently — none blocks the loop.
        cfg = MachineConfig(image="alpine:3.20", ports=[PortSpec(host=8080, guest=8080)])
        conn = ConnectOptions(target="cloud")
        machines = await asyncio.gather(*(AsyncMachine.create(cfg, conn) for _ in range(8)))
        try:
            await asyncio.gather(*(m.wait_until_ready() for m in machines))
            outs = await asyncio.gather(*(m.exec(["echo", "hi"]) for m in machines))
        finally:
            await asyncio.gather(*(m.delete() for m in machines))

    asyncio.run(main())
"""

from __future__ import annotations

import asyncio
from typing import AsyncIterator, Optional

from .machine import Machine
from .types import (
    ConnectOptions,
    ExecOptions,
    ExecResult,
    ImageInfo,
    MachineConfig,
    PortEndpoint,
    PortSpec,
)

__all__ = ["AsyncMachine"]


class AsyncMachine:
    """An awaitable view of a :class:`Machine`. Construct with :meth:`create`
    (or :meth:`connect`); clean up with :meth:`delete` (or ``async with``).

    Every I/O method mirrors :class:`Machine` but is a coroutine that runs the
    underlying blocking call in a thread — so a single event loop can create,
    drive, and tear down many machines at once without blocking."""

    def __init__(self, machine: Machine) -> None:
        self._m = machine

    @classmethod
    async def create(
        cls,
        config: Optional[MachineConfig] = None,
        conn: Optional[ConnectOptions] = None,
    ) -> "AsyncMachine":
        """Create and start a machine (awaits readiness, off the event loop)."""
        m = await asyncio.to_thread(Machine.create, config, conn)
        return cls(m)

    @classmethod
    async def connect(
        cls,
        machine_id: str,
        conn: Optional[ConnectOptions] = None,
    ) -> "AsyncMachine":
        """Attach to an EXISTING machine without creating a new one."""
        m = await asyncio.to_thread(Machine.connect, machine_id, conn)
        return cls(m)

    @property
    def name(self) -> str:
        """The machine's name / identifier."""
        return self._m.name

    async def state(self) -> str:
        """Current state, e.g. ``"running"`` / ``"stopped"``."""
        return await asyncio.to_thread(self._m.state)

    async def ready(self) -> bool:
        """Whether the machine is READY to do work (see :meth:`Machine.ready`)."""
        return await asyncio.to_thread(self._m.ready)

    async def ready_at(self) -> Optional[str]:
        """When the machine first became ready (RFC3339), or ``None``."""
        return await asyncio.to_thread(self._m.ready_at)

    async def wait_until_ready(self, timeout_s: float = 120.0, interval_s: float = 1.0) -> None:
        """Await readiness (or raise on a failed/stopped state or timeout)."""
        await asyncio.to_thread(self._m.wait_until_ready, timeout_s, interval_s)

    def endpoint(self, port: int, path: Optional[str] = None) -> PortEndpoint:
        """Build an authenticated connect-bridge endpoint (URL + headers) for a
        published guest port. Pure URL construction — no I/O — so it is a plain
        (non-awaitable) method. See :meth:`Machine.endpoint`. (cloud)"""
        return self._m.endpoint(port, path)

    async def request(
        self,
        port: int,
        path: Optional[str] = None,
        method: str = "GET",
        data: Optional[bytes] = None,
        timeout_s: float = 30.0,
    ) -> bytes:
        """Authenticated HTTP request to a published guest port via the connect
        bridge; returns the raw response body bytes. (cloud)"""
        return await asyncio.to_thread(self._m.request, port, path, method, data, timeout_s)

    async def url(self) -> Optional[str]:
        """Public ingress URL for the machine's first published port (cloud)."""
        return await asyncio.to_thread(self._m.url)

    async def exec(self, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        """Execute a command directly in the machine."""
        return await asyncio.to_thread(self._m.exec, command, opts)

    async def run(self, image: str, command: list[str], opts: Optional[ExecOptions] = None) -> ExecResult:
        """Pull an image (if needed) and run a command in a container. (local)"""
        return await asyncio.to_thread(self._m.run, image, command, opts)

    async def exec_stream(
        self, command: list[str], opts: Optional[ExecOptions] = None
    ) -> AsyncIterator[dict]:
        """Execute a command and yield its output events LIVE. (local)

        Bridges the sync generator to async by pulling each event off the event
        loop, so streaming never blocks other coroutines."""
        sync_gen = self._m.exec_stream(command, opts)
        sentinel = object()

        def _next():
            try:
                return next(sync_gen)
            except StopIteration:
                return sentinel

        while True:
            event = await asyncio.to_thread(_next)
            if event is sentinel:
                return
            yield event

    async def read_file(self, path: str) -> bytes:
        """Read a file from the machine."""
        return await asyncio.to_thread(self._m.read_file, path)

    async def write_file(self, path: str, data: bytes | str, mode: Optional[int] = None) -> None:
        """Write a file into the machine."""
        await asyncio.to_thread(self._m.write_file, path, data, mode)

    async def pull_image(self, image: str) -> ImageInfo:
        """Pull an OCI image into the machine's storage. (local)"""
        return await asyncio.to_thread(self._m.pull_image, image)

    async def list_images(self) -> list[ImageInfo]:
        """List cached OCI images. (local)"""
        return await asyncio.to_thread(self._m.list_images)

    async def stop(self) -> None:
        """Stop the machine."""
        await asyncio.to_thread(self._m.stop)

    async def delete(self) -> None:
        """Stop the machine and delete its storage."""
        await asyncio.to_thread(self._m.delete)

    async def fork(self, name: str, ports: Optional[list[PortSpec]] = None) -> "AsyncMachine":
        """Fork this running, forkable machine into a new clone. (cloud/local)"""
        clone = await asyncio.to_thread(self._m.fork, name, ports)
        return AsyncMachine(clone)

    # -- async context manager: auto-delete on exit --
    async def __aenter__(self) -> "AsyncMachine":
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.delete()
