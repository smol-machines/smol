"""Scoped loopback access to published TCP services, locally or through cloud."""

from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
from dataclasses import dataclass
from urllib.parse import quote, urlsplit, urlunsplit


@dataclass(frozen=True)
class TunnelEndpoint:
    host: str
    port: int


@asynccontextmanager
async def open_tunnel(machine, port: int):
    """Keep a private loopback listener alive for the duration of this context.

    Cloud requires ``smolmachines[tunnel]`` and a server supporting raw TCP
    tunnels. Closing the context also closes every accepted connection.
    """
    if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535:
        raise ValueError("port must be between 1 and 65535")
    transport = machine._t
    from .transport import CloudTransport

    cloud = isinstance(transport, CloudTransport)
    if cloud:
        from websockets.asyncio.client import connect

        base = urlsplit(transport._base)
        if base.scheme not in {"http", "https"} or base.username or base.password:
            raise ValueError("cloud tunnel requires an HTTP(S) API endpoint")
        url = urlunsplit((
            "wss" if base.scheme == "https" else "ws", base.netloc,
            base.path.rstrip("/") + "/v1/machines/" + quote(transport._id, safe="")
            + f"/tunnel/{port}", "", "",
        ))
    else:
        endpoint = transport.endpoint(port)
        address = urlsplit(endpoint.http_url)
        if address.hostname != "127.0.0.1" or address.port is None:
            raise ValueError("local machine port must resolve to loopback")

    clients: set[asyncio.Task] = set()

    async def relay(reader, writer):
        try:
            if cloud:
                async with connect(
                    url, additional_headers={"Authorization": f"Bearer {transport._key}"},
                    open_timeout=10, close_timeout=2, max_size=65536, max_queue=4,
                    proxy=None,
                ) as websocket:
                    async def upload():
                        while chunk := await reader.read(32768):
                            await websocket.send(chunk)

                    async def download():
                        async for chunk in websocket:
                            if not isinstance(chunk, bytes):
                                raise ValueError("TCP tunnel received a non-binary frame")
                            writer.write(chunk)
                            await writer.drain()

                    await _until_closed(upload(), download())
            else:
                remote_reader, remote_writer = await asyncio.wait_for(
                    asyncio.open_connection(address.hostname, address.port), 10
                )
                try:
                    await _until_closed(
                        _copy(reader, remote_writer), _copy(remote_reader, writer)
                    )
                finally:
                    remote_writer.close()
                    await remote_writer.wait_closed()
        except (OSError, TimeoutError):
            # A failed upstream closes this SSH connection. The caller observes
            # failure through SSH; never log credential-bearing HTTP headers.
            pass
        finally:
            writer.close()
            await writer.wait_closed()

    def accepted(reader, writer):
        task = asyncio.create_task(relay(reader, writer))
        clients.add(task)
        def finished(done):
            clients.discard(done)
            if not done.cancelled():
                done.exception()  # Connection errors must not leak auth headers.
        task.add_done_callback(finished)

    server = await asyncio.start_server(accepted, "127.0.0.1", 0, limit=65536)
    try:
        yield TunnelEndpoint("127.0.0.1", server.sockets[0].getsockname()[1])
    finally:
        server.close()
        pending = list(clients)
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        await server.wait_closed()


async def _copy(reader, writer):
    while chunk := await reader.read(32768):
        writer.write(chunk)
        await writer.drain()


async def _until_closed(*operations):
    tasks = [asyncio.create_task(operation) for operation in operations]
    try:
        done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            task.result()
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
