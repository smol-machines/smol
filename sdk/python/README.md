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

# Cloud (smolfleet) — create() waits until it is ready for work.
from smol import ConnectOptions
m = Machine.create(
    MachineConfig(image="alpine:3.20"),
    ConnectOptions(target="cloud"),  # uses SMOL_CLOUD_TOKEN
)
try:
    print(m.exec(["echo", "ready for work"]).stdout)
finally:
    m.delete()
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

### Fused multi-policy rollouts

`RolloutClient` is the thin generation boundary for TRL, Unsloth, and custom RL
loops. The node keeps one vLLM engine hot, verifies immutable LoRA versions, and
submits cross-policy cohorts concurrently so vLLM can continuously batch them.

```python
from smol import RolloutClient

rollouts = RolloutClient("http://127.0.0.1:8080/api/v1", "qwen")
rollouts.ensure_vllm_executor(
    endpoint="http://127.0.0.1:8000",
    adapter_root="/var/lib/smol/adapters",
    fallback_pool="isolated-rollouts",
)
rollouts.publish_policy("experiment-a", "step-40", "/var/lib/smol/adapters/a-40")
result = rollouts.generate(
    idempotency_key="experiment-a-step-40-batch-7",
    policy="experiment-a",
    prompts=[[1, 2, 3]],
    max_tokens=64,
    temperature=0.9,
    logprobs=1,
)
```

Inside a forked rollout worker, no configuration is required: `RolloutClient()`
discovers its authenticated node assignment from `/etc/smolvm/fork-env` and
automatically groups workers from the same fork batch into a bounded cohort.
Pass `auto_fork_cohort=False` only when the application already supplies an
explicit `cohort_id`, `cohort_size`, and `cohort_max_wait_ms`.

Optional framework adapters are explicit imports, so the base SDK remains free
of PyTorch, PEFT, Transformers, Unsloth, and vLLM dependencies:

```python
from smol.integrations import (
    UnslothVllmExecutor,
    add_transformers_forkpoint,
    publish_peft_adapter,
)
```

The vLLM backend must bind to loopback, enable runtime LoRA updates, and reserve
one spare CPU LoRA slot so a new version can load before the old version drains.

### NeMo Gym sandbox provider

Install the optional integration and select the same `smol` provider for local
SmolVM or Smol Cloud:

```bash
pip install 'smolmachines[nemo-gym]'
```

```yaml
sandbox:
  smol:
    target: local                 # or cloud; cloud reuses `smol auth login`
    checkpoints:
      ghcr.io/acme/swe:ready:
        machine: swe-golden       # running MachineConfig(forkable=True) machine
        ports: [8000]
        resources:                # describe the prepared golden's capacity
          cpu: 4
          memory_mib: 8192
          disk_gib: 20
        provider_options:         # describe its inherited egress policy
          allow_hosts: [api.example.com]
    fork_batch_window_ms: 2        # coalesce concurrent episode creates
    fork_batch_size: 32
  default_metadata:
    sandbox-api: smol
```

NeMo Gym discovers the provider through its standard
`nemo_gym.sandbox_providers` entry point. A normal image creates a fresh
microVM. An exact image match in `checkpoints` instead creates every episode as
a live RAM/disk copy-on-write fork of that prepared machine, so repositories,
dependencies, services, and caches can already be running when the agent takes
its first action. The provider implements exec, files, resource limits, scoped
egress, declared service ports, entrypoint overrides, TTL cleanup, and the same
configuration for local and cloud targets. Handles serialize without credentials
and reconnect through the receiving SDK session, as required by DeepSWE's
agent/verifier lifecycle. Cloud checkpoint episodes with a TTL use Smol Cloud's
durable lease controller, so they are reclaimed even if the NeMo Gym process
exits unexpectedly.

Checkpoint forks inherit the golden's resource shape, network policy, and running
workload. Declare those inherited properties in the checkpoint mapping. A task's
resource request may be smaller than the declared capacity, while its network
policy, entrypoint, and ports must match exactly; incompatible requests fail
instead of silently running with different isolation. NeMo Gym's standard
sandbox provider contract supports episode-from-golden forks.

To branch an arbitrary live trajectory state, create that source as branchable
and call the Smol provider's extension at the decision point:

```python
from nemo_gym.sandbox.providers.base import SandboxSpec
from smol.nemo_gym import SmolProvider

provider = SmolProvider(target="local")  # or target="cloud"
checkpoint = await provider.create(
    SandboxSpec(
        image="ghcr.io/acme/swe:ready",
        provider_options={"branchable": True},
    )
)
await provider.exec(checkpoint, "./agent-step-1")
await provider.exec(checkpoint, "./agent-step-2")

branches = await provider.branch(checkpoint, count=16, name_prefix="candidate")
```

The first fan-out waits for active commands and freezes the source at that exact
RAM/filesystem/process state. Every returned sandbox is an independent COW leaf;
the frozen source may create more siblings from the same state but cannot execute
more commands. Close the branches before the source. A configured checkpoint
fork is already a leaf, so it cannot request another live branch until SmolVM
supports nested fork generations.

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
m = Machine.create(
    MachineConfig(image="alpine:3.20", ports=[PortSpec(host=8080, guest=8080)]),
    ConnectOptions(target="cloud"),
)
try:
    # create() already waited: the guest agent is reachable and the published
    # port is accepting connections.
    # Reach a service INSIDE the vm through the authenticated connect bridge —
    # no Cloudflare/localhost.run tunnel, no public exposure, no egress allow-list.
    # Have the worker LISTEN on a published port and connect *inbound*:
    print(m.request(8080, "healthz").decode())     # authed HTTP to the guest port
    ep = m.endpoint(8080, "/socket")               # or build a ws:// url for your ws client
    # websocket.connect(ep.ws_url, additional_headers=ep.headers)
finally:
    m.delete()

# Machine.connect() intentionally does not wait for readiness:
existing = Machine.connect(machine_id, ConnectOptions(target="cloud"))
existing.wait_until_ready()
```

## API
- `Machine` (sync) / `AsyncMachine` (awaitable, non-blocking) — identical surface; see the async example above.
- `RolloutClient` — publish versioned LoRAs and generate single- or multi-policy cohorts.
- `Machine.create(config=None, conn=None)` — create and start a machine; cloud
  waits for `ready is True` before returning.
- `Machine.connect(machine_id, conn=None)` — attach without waiting; call
  `wait_until_ready()` before use.
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
