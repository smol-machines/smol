# Atomic local TCP publication

`Machine.publish_atomic(guest)` binds an OS-allocated IPv4 loopback listener in
the running local engine and returns its `PublishedPort`. The listener remains
owned from allocation through publication. `machine.endpoint(guest)` returns
the local HTTP/WebSocket endpoint using the saved mapping.

```python
from smol import ConnectOptions, Machine, MachineConfig, ResourceSpec

with Machine.create(
    MachineConfig(
        image="your-image:tag",
        resources=ResourceSpec(network=False),
        wait_for_ports=False,
    ),
    ConnectOptions(target="local"),
) as machine:
    mapping = machine.publish_atomic(8000)
    print(mapping.host, machine.endpoint(8000).http_url)
```

The image must listen on guest port 8000. Guest outbound networking is independent
of this inbound control connection. The operation requires a running machine and
a guest TCP port in 1..65535. It always binds `127.0.0.1`, including on a host with
`SMOLVM_PUBLISH_ADDR` configured. Use `unpublish` to remove the mapping, or delete
the machine to close all its listeners. `AsyncMachine.publish_atomic` provides
the same operation without blocking its event loop.

This Python API requires the accompanying smolvm engine implementation. An older
native extension produces an installation error; an older boot helper reports
that atomic port allocation is unsupported. Cloud execution is rejected before
any network request. Existing explicit and automatic `publish` behavior is
unchanged.

Validation:

```bash
PYTHONPATH=sdk/python/python python -m pytest sdk/python/tests/test_atomic_ports.py
SMOL_RUN_LOCAL_E2E=1 PYTHONPATH=sdk/python/python \
  python -m pytest sdk/python/tests/test_atomic_ports_e2e.py
```

The local test boots a published Alpine image with outbound networking disabled,
publishes two continuously held ports, sends real HTTP requests and verifies
that deletion removes the exact owned machine. The OpenHands integration needs
a published wheel containing this API and engine before its dependency minimum
can be finalized.
