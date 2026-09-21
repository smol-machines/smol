import asyncio
from types import SimpleNamespace

import pytest

from smol.tunnel import open_tunnel
from smol.transport import CloudTransport


def test_local_tunnel_roundtrip_and_cleanup():
    async def run():
        async def echo(reader, writer):
            try:
                while data := await reader.read(32768):
                    writer.write(data)
                    await writer.drain()
            finally:
                writer.close()
                await writer.wait_closed()

        upstream = await asyncio.start_server(echo, "127.0.0.1", 0)
        port = upstream.sockets[0].getsockname()[1]
        machine = SimpleNamespace(_t=SimpleNamespace(
            endpoint=lambda _: SimpleNamespace(http_url=f"http://127.0.0.1:{port}")
        ))
        try:
            async with open_tunnel(machine, 22222) as endpoint:
                reader, writer = await asyncio.open_connection(endpoint.host, endpoint.port)
                payload = bytes(range(256)) * 128
                writer.write(payload)
                await writer.drain()
                assert await reader.readexactly(len(payload)) == payload
            assert await asyncio.wait_for(reader.read(), 2) == b""
            writer.close()
            await writer.wait_closed()
            with pytest.raises(ConnectionRefusedError):
                await asyncio.open_connection(endpoint.host, endpoint.port)
        finally:
            upstream.close()
            await upstream.wait_closed()

    asyncio.run(asyncio.wait_for(run(), 10))


@pytest.mark.parametrize("port", [0, 65536, True, "22"])
def test_invalid_tunnel_port(port):
    async def run():
        with pytest.raises(ValueError, match="port must"):
            async with open_tunnel(None, port):
                pytest.fail("invalid port accepted")
    asyncio.run(run())


def test_cloud_tunnel_binary_auth_and_disconnect():
    from websockets.asyncio.server import serve

    async def run():
        disconnected = asyncio.Event()
        async def echo(ws):
            assert ws.request.path == "/v1/machines/mach-test/tunnel/22222"
            assert ws.request.headers["Authorization"] == "Bearer test-only-key"
            try:
                async for data in ws:
                    assert isinstance(data, bytes)
                    await ws.send(data)
            finally:
                disconnected.set()
        async with serve(echo, "127.0.0.1", 0) as upstream:
            port = upstream.sockets[0].getsockname()[1]
            machine = SimpleNamespace(_t=CloudTransport(
                f"http://127.0.0.1:{port}", "test-only-key", "mach-test", "test"
            ))
            async with open_tunnel(machine, 22222) as endpoint:
                reader, writer = await asyncio.open_connection(endpoint.host, endpoint.port)
                payload = bytes(range(256)) * 128
                writer.write(payload)
                await writer.drain()
                assert await reader.readexactly(len(payload)) == payload
            assert await reader.read() == b""
            writer.close()
            await writer.wait_closed()
            await disconnected.wait()
    asyncio.run(asyncio.wait_for(run(), 10))
