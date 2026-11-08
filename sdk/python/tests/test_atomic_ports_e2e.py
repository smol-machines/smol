"""Opt-in local VM test for atomic loopback publication.

SMOL_RUN_LOCAL_E2E=1 PYTHONPATH=sdk/python/python python -m pytest \
    sdk/python/tests/test_atomic_ports_e2e.py
"""

import os
import socket
import urllib.request
import uuid

import pytest

from smol import ConnectOptions, Machine, MachineConfig, ResourceSpec


@pytest.mark.skipif(os.environ.get("SMOL_RUN_LOCAL_E2E") != "1", reason="local VM opt-in")
def test_atomic_port_reaches_guest_without_outbound_network():
    name = "atomic-port-test-" + uuid.uuid4().hex
    connection = ConnectOptions(target="local")
    try:
        machine = Machine.create(MachineConfig(
            name=name, image="alpine:3.20", persistent=False, detach=False,
            resources=ResourceSpec(cpus=1, memory_mb=512, storage_gb=2, network=False),
            command=["/bin/sh", "-c", "while true; do "
                     "printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 2\\r\\n\\r\\nok' "
                     "| nc -l -p 8000; done"],
            wait_for_ports=False,
        ), connection)
        first = machine.publish_atomic(8000)
        second = machine.publish_atomic(8000)
        assert first.host != second.host
        assert first.host and second.host
        for mapping in (first, second):
            with socket.socket() as contender:
                with pytest.raises(OSError):
                    contender.bind(("127.0.0.1", mapping.host))
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(f"http://127.0.0.1:{mapping.host}", timeout=5) as response:
                assert response.status == 200 and response.read() == b"ok"
        assert machine.endpoint(8000).http_url.startswith("http://127.0.0.1:")
    finally:
        Machine.delete_by_name(name, connection, drain_secs=0)
    assert name not in {m.name for m in Machine.list(connection)}
