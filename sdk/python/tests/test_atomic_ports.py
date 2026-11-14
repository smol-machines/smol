"""Atomic local publication contracts without starting a VM."""

import asyncio
import socket
from contextlib import ExitStack

import pytest

from smol import AsyncMachine, Machine
from smol.errors import InvalidConfigError, NotSupportedError
from smol.transport import CloudTransport, LocalTransport


class NativePublisher:
    def __init__(self, stack):
        self.stack = stack
        self.listeners = {}

    def publish_atomic(self, guest):
        listener = self.stack.enter_context(socket.socket())
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        host = listener.getsockname()[1]
        self.listeners[guest] = (host, listener)
        return host

    def host_port(self, guest):
        return self.listeners[guest][0]

    def published_ports(self):
        return [(host, guest, "tcp") for guest, (host, _) in self.listeners.items()]


def test_atomic_publication_returns_bound_endpoint():
    with ExitStack() as stack:
        machine = Machine(LocalTransport(NativePublisher(stack), cleanup_on_exit=False))
        first = machine.publish_atomic(8000)
        second = machine.publish_atomic(8001)
        assert first.host != second.host
        assert first.host > 0
        assert machine.endpoint(8000).http_url == f"http://127.0.0.1:{first.host}"
        for mapping in [first, second]:
            with socket.socket() as contender:
                with pytest.raises(OSError):
                    contender.bind(("127.0.0.1", mapping.host))
        assert machine.ports() == [first, second]


@pytest.mark.parametrize("guest", [0, -1, 65536, True, "8000"])
def test_invalid_guest_never_allocates(guest):
    with ExitStack() as stack:
        native = NativePublisher(stack)
        machine = Machine(LocalTransport(native, cleanup_on_exit=False))
        with pytest.raises(InvalidConfigError):
            machine.publish_atomic(guest)
        assert native.listeners == {}


def test_cloud_is_rejected_without_network_access():
    machine = Machine(CloudTransport("http://127.0.0.1:9", "unused", "id", "name"))
    with pytest.raises(NotSupportedError, match="local-only"):
        machine.publish_atomic(8000)


def test_old_native_engine_is_actionable():
    machine = Machine(LocalTransport(object(), cleanup_on_exit=False))
    with pytest.raises(NotSupportedError, match="reinstall"):
        machine.publish_atomic(8000)


def test_async_publication():
    with ExitStack() as stack:
        machine = Machine(LocalTransport(NativePublisher(stack), cleanup_on_exit=False))
        mapping = asyncio.run(AsyncMachine(machine).publish_atomic(8000))
        assert mapping.host > 0
        assert machine.endpoint(8000).http_url.endswith(f":{mapping.host}")
