# smol — Python SDK

Embed isolated **microVM sandboxes** directly in your Python code. Same API
locally (embedded engine, no server) or against the **smolfleet cloud** — the
backend is chosen via `ConnectOptions` / `SMOL_CLOUD_TOKEN`. Mirrors the
[Node SDK](../node).

> **Supported platforms** (native *local* transport): macOS **Apple Silicon**, and
> **Linux x64/arm64 with glibc ≥ 2.34** (RHEL 9, Ubuntu 22.04+, Debian 12, Amazon
> Linux 2023; the wheel is tagged `manylinux_2_34`). The **cloud** transport works
> anywhere the wheel installs. Not yet published: macOS Intel, and glibc < 2.34.

```python
from smol import Machine, MachineConfig, ResourceSpec

# Local (embedded microVM) — boots in-process, no server.
with Machine.create(MachineConfig(resources=ResourceSpec(cpus=2, memory_mb=1024, network=True))) as m:
    res = m.run("python:3.12", ["python", "-c", "print(2 ** 10)"])
    res.assert_success()
    print(res.stdout)            # 1024
    m.write_file("/tmp/in.txt", "hi")
    print(m.read_file("/tmp/in.txt").decode())

# Cloud (smolfleet) — same API, just point at the cloud.
from smol import ConnectOptions
with Machine.create(
    MachineConfig(image="alpine:3.20"),
    ConnectOptions(target="cloud", api_key="smk_…"),  # or set SMOL_CLOUD_TOKEN
) as m:
    print(m.exec(["echo", "hi"]).stdout)
```

### Async: `AsyncMachine` (non-blocking)

`Machine` is synchronous — each call blocks the calling thread. When you're
driving many machines from one event loop (a fleet of disposable workers), use
`AsyncMachine`: the **same API**, but every I/O method is a coroutine that runs
off the loop, so launches and calls overlap instead of serializing.

```python
import asyncio
from smol import AsyncMachine, MachineConfig, ConnectOptions, PortSpec

async def main():
    cfg = MachineConfig(image="alpine:3.20", ports=[PortSpec(host=8080, guest=8080)])
    conn = ConnectOptions(target="cloud")  # or SMOL_CLOUD_TOKEN
    # Launch a fleet concurrently — none blocks the loop.
    machines = await asyncio.gather(*(AsyncMachine.create(cfg, conn) for _ in range(8)))
    try:
        await asyncio.gather(*(m.wait_until_ready() for m in machines))
        # Reach a service inside a vm via the authed connect bridge (no tunnel):
        health = await machines[0].request(8080, "healthz")
    finally:
        await asyncio.gather(*(m.delete() for m in machines))

asyncio.run(main())
```

Every `Machine` method has an `await`able counterpart on `AsyncMachine`
(`create`/`connect`/`exec`/`wait_until_ready`/`request`/`fork`/…), plus
`async with` for auto-delete. `endpoint(port)` stays synchronous — it only builds
a URL and does no I/O.

## Architecture
- **Pure-Python layer** (`python/smol`): `Machine`, transports, types, errors —
  zero third-party deps (the cloud transport uses only `urllib`).
- **Native core** (`src/lib.rs`, crate `smol-py`): a `pyo3` extension that links
  the `smolvm` engine in-process for the local path — the Python analogue of the
  `smol-node` NAPI crate. The local API is **synchronous** (the engine blocks).
- **Cloud transport**: a REST client to smolfleet `/v1` whose request/response
  shapes match smolfleet's OpenAPI contract (Bearer `smk_…`).

### Disposable workers: wait for `ready`, then connect (cloud)

Launching a machine as a **disposable agent runtime** has two easy-to-miss steps;
both are first-class here.

`Machine.create()` already waits for the machine to be **ready** — not merely
`started`. `state == "started"` means the VM process launched; the guest is
still booting and is **not** usable yet. Acting on `started` is the classic
teardown race (works on a slow cold start, times out on a warm one). Gate on the
unambiguous signal:

```python
with Machine.create(
    MachineConfig(image="alpine:3.20", ports=[PortSpec(host=8080, guest=8080)]),
    ConnectOptions(target="cloud"),
) as m:
    m.wait_until_ready()           # create() already waited; re-assert if reconnecting
    print(m.ready(), m.ready_at())

    # Reach a service INSIDE the vm through the authenticated connect bridge —
    # no Cloudflare/localhost.run tunnel, no public exposure, no egress allow-list.
    # Have the worker LISTEN on a published port and connect *inbound*:
    print(m.request(8080, "healthz").decode())     # authed HTTP to the guest port
    ep = m.endpoint(8080, "/socket")               # or build a ws:// url for your ws client
    # websocket.connect(ep.ws_url, additional_headers=ep.headers)
```

## API
- `Machine` (sync) / `AsyncMachine` (awaitable, non-blocking) — identical surface; see the async example above.
- `Machine.create(config=None, conn=None)` — create and start a machine.
- `machine.exec(command, opts=None)` / `machine.run(image, command, opts=None)` → `ExecResult`
- `machine.read_file(path)` → `bytes` / `machine.write_file(path, data, mode=None)`
- `machine.ready()` / `machine.ready_at()` / `machine.wait_until_ready(timeout_s=120, interval_s=1)`  *(cloud)*
- `machine.endpoint(port, path=None)` → `PortEndpoint` / `machine.request(port, path=None, method="GET", data=None)` → `bytes`  *(cloud connect bridge)*
- `machine.pull_image(image)` / `machine.list_images()`  *(local)*
- `machine.stop()` / `machine.delete()` / `machine.state()`
- Use it as a context manager to auto-`delete()` on exit.
- Errors are typed: `SmolError` (with `.code`), `ExecutionError`,
  `NotSupportedError`, `InvalidConfigError`.

`ExecResult` has `.exit_code`, `.stdout`, `.stderr`, `.success`, `.output`, and
`.assert_success()`.

## Install / build from source
The cloud path is pure Python. The local path needs the native extension, which
links `libkrun` from the sibling `smolvm` repo (three levels up).

```bash
python -m venv .venv && . .venv/bin/activate
pip install maturin
# Build + install the native extension (points at the repo's bundled libkrun):
LIBKRUN_BUNDLE=../../../lib maturin develop
```

To boot local microVMs the engine needs a code-signed boot helper carrying the
macOS `com.apple.security.hypervisor` entitlement (the Python process itself does
not). Point it at one (and the libkrun dir):

```bash
SMOLVM_BOOT_BINARY=../../../target/release/smolvm \
SMOLVM_LIB_DIR=../../../lib \
python your_script.py
```

On Linux the host needs `/dev/kvm`.

## Tests
```bash
python tests/test_unit.py        # error parsing + path encoding (no VM/network)
python tests/test_cloud_mock.py  # cloud transport vs a mock /v1 (no VM/network)
python tests/test_async_mock.py  # AsyncMachine vs a mock /v1 (concurrency, no VM/network)
# Local VM boot (needs the native build + the env above):
SMOLVM_BOOT_BINARY=… SMOLVM_LIB_DIR=… .venv/bin/python tests/test_local_e2e.py
```

## License
Apache-2.0
