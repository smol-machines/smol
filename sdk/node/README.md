# smol (Node SDK)

Embed isolated **microVM sandboxes** directly in your Node.js code — no server to
run. The smolvm engine is linked in-process via a native addon.

> **Supported platforms** (native *local* transport): macOS **Apple Silicon**, and
> **Linux x64/arm64 with glibc ≥ 2.34** (RHEL 9, Ubuntu 22.04+, Debian 12, Amazon
> Linux 2023). The **cloud** transport works anywhere the package installs.
> Not yet prebuilt: macOS Intel, and Linux with glibc < 2.34.

Run the **same code** against the local embedded engine or the smolfleet **cloud** —
the backend is chosen by `ConnectOptions`:

```ts
// Local (embedded, default) — no server, no config:
const local = await Machine.create({ resources: { cpus: 2, memoryMb: 1024 } });

// Cloud (smolfleet) — pass an API key, or set SMOL_CLOUD_TOKEN.
const cloud = await Machine.create(
  { image: 'python:3.12' },
  { target: 'cloud' }, // uses SMOL_CLOUD_TOKEN
);
try {
  // create() returned only after the guest agent became reachable.
  const res = await cloud.exec(['python', '-c', 'print(40 + 2)']);
  console.log(res.stdout);
} finally {
  await cloud.delete();
}
```

Cloud-only gaps (`run`, `execStream`, `pullImage`, `listImages`) throw `NotSupportedError`;
the common surface (create/exec/files/state/stop/delete) is identical on both.

### Disposable workers: wait for `ready`, then connect (cloud)

Launching a machine as a **disposable agent runtime** has two easy-to-miss steps;
both are first-class here.

`Machine.create()` already waits for the machine to be **ready** — not merely
`started`. `state === "started"` means the VM process launched; the guest is
still booting and is **not** usable yet. Acting on `started` is the classic
teardown race (works on a slow cold start, times out on a warm one). Gate on the
unambiguous signal:

```ts
const m = await Machine.create(
  { image, ports: [{ host: 8080, guest: 8080 }] },
  { target: 'cloud' },
);
try {
  // create() has already waited: the guest agent is reachable and the
  // published port is accepting connections.
  const res = await m.fetch(8080, '/healthz');
  console.log(await res.text());
} finally {
  await m.delete();
}
```

To reach a service **inside** the VM, use the authenticated connect bridge —
**no Cloudflare/localhost.run tunnel, no public exposure, no egress allow-list.**
Have the worker LISTEN on a published port and connect *inbound*:

```ts
// Machine.connect() does not wait. Explicitly gate a pre-existing machine:
const existing = await Machine.connect(machineId, { target: 'cloud' });
await existing.waitUntilReady();

// Or a WebSocket, using your own ws client with the authed endpoint:
const { wsUrl, headers } = existing.endpoint(8080, '/socket');
const ws = new WebSocket(wsUrl, { headers });   // e.g. the `ws` package
```

## Install

```bash
npm install smolmachines
```

Requires Node.js ≥ 18 on a host the engine supports (macOS Apple Silicon, or Linux
with KVM).

Bun 1.3.14 and newer uses the same API, including zero-configuration local
machines—the package automatically configures its bundled hypervisor, boot
helper, and guest rootfs:

```bash
bun add smolmachines
bun run app.ts
```

## Fused multi-policy rollouts

```ts
import { RolloutClient } from 'smolmachines';

const rollouts = new RolloutClient('http://127.0.0.1:8080/api/v1', 'qwen');
await rollouts.ensureVllmExecutor({
  endpoint: 'http://127.0.0.1:8000',
  adapterRoot: '/var/lib/smol/adapters',
  fallbackPool: 'isolated-rollouts',
});
await rollouts.publishPolicy('experiment-a', 'step-40', '/var/lib/smol/adapters/a-40');
const result = await rollouts.generate({
  idempotencyKey: 'experiment-a-step-40-batch-7',
  policy: 'experiment-a',
  prompts: [[1, 2, 3]],
  sampling: { maxTokens: 64, temperature: 0.9, logprobs: 1 },
});
```

Inside a forked rollout worker, `new RolloutClient()` discovers its authenticated
node assignment from `/etc/smolvm/fork-env` and automatically groups workers
from the same fork batch into a bounded cohort. Set `autoForkCohort: false` only
when the application already supplies an explicit `cohort`.

The client targets the loopback rollout API on a CUDA node; it publishes
content-verified LoRA versions and submits cross-policy cohorts without exposing
vLLM's unrestricted adapter loader.

## Usage

```ts
import { Machine } from 'smolmachines';

const m = await Machine.create({ resources: { cpus: 2, memoryMb: 1024 } });
try {
  // Run a command in a container image
  const res = await m.run('python:3.12', ['python', '-c', 'print(2 ** 10)']);
  res.assertSuccess();
  console.log(res.stdout); // "1024\n"

  // Or exec directly in the VM, move files in/out
  await m.writeFile('/tmp/hello.txt', 'hi');
  const back = await m.readFile('/tmp/hello.txt');
  console.log(back.toString()); // "hi"
} finally {
  await m.delete();
}
```

## API

- `Machine.create(config?, conn?)` — create and start a machine; cloud waits for
  `ready === true` before returning.
- `Machine.connect(id, conn?)` — attach to an existing machine without waiting;
  call `waitUntilReady()` before use.
- `machine.ready()` / `machine.readyAt()` /
  `machine.waitUntilReady({ timeoutMs, intervalMs })` *(cloud)*.
- `machine.exec(command, opts?)` / `machine.run(image, command, opts?)` → `ExecResult`.
- `machine.execStream(command, opts?)` → `AsyncGenerator<ExecEvent>`.
- `machine.readFile(path)` / `machine.writeFile(path, data, mode?)`.
- `machine.pullImage(image)` / `machine.listImages()`.
- `machine.stop()` / `machine.delete()` / `await machine.state()`. Cloud
  `"started"` means VM launched, not ready for work.

Errors are typed: `SmolError` (with `.code`), `ExecutionError`, `NotSupportedError`, `InvalidConfigError`.

## Building from source

This package's native core lives alongside it (Rust, `src/*.rs`) and links the
sibling `smolvm` repo's engine + `libkrun`. From this directory:

```bash
npm install
npm run build        # napi build (native) + tsc (types) + bundle
```

The native build needs the Rust toolchain, `@napi-rs/cli`, and `libkrun` available
in the `smolvm` repo's `lib/` (this package expects the `smolvm` repo checked out
three levels up).

## License

Apache-2.0
