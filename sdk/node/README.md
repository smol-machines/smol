# smol (Node SDK)

Embed isolated **microVM sandboxes** directly in your Node.js code — no server to
run. The SDK is linked into your process via a native addon; the VMM itself is
a separate `smol-vmm` helper (seccomp- and Landlock-confined on Linux), so a
guest escape lands there and not in your application.

> **Supported platforms** (native *local* transport): macOS **Apple Silicon**, and
> **Linux x64/arm64 with glibc ≥ 2.34** (RHEL 9, Ubuntu 22.04+, Debian 12, Amazon
> Linux 2023). The **cloud** transport works anywhere the package installs.
> Not yet prebuilt: macOS Intel, and Linux with glibc < 2.34.
>
> The local engine for your platform (native addon, boot helper, hypervisor
> libraries and guest rootfs) installs automatically as an optional dependency,
> e.g. `smolmachines-linux-x64-gnu`, so an install downloads only your
> platform's runtime. Using only the cloud transport? Install with
> `npm install --omit=optional smolmachines` to skip it entirely.

Run the **same code** against the local embedded engine or **smol cloud** —
the backend is chosen by `ConnectOptions`:

```ts
// Local (embedded, default) — no server, no config:
const local = await Machine.create({ resources: { cpus: 2, memoryMb: 1024 } });

// A trusted local interceptor can inspect outbound TCP. Keep its token out of logs.
const intercepted = await Machine.create({
  network: true,
  egressInterceptor: { address: '127.0.0.1:9000', token: process.env.SMOLVM_INTERCEPTOR_TOKEN! },
});
// Supply the binding again when connecting from a new process:
// await Machine.connect(intercepted.name, { target: 'local', egressInterceptor: { address: '127.0.0.1:9000', token: process.env.SMOLVM_INTERCEPTOR_TOKEN! } });

// Branch a prepared machine: a CoW clone of its RAM and disks, typically
// under 200ms, so a warm environment is reused instead of rebuilt. Pass
// `network: true` whenever an image has to be pulled.
const source = await Machine.create({ image: 'alpine', network: true, branchable: true });
const branch = await source.branch('b1');

// Periodic local rollback points reuse unchanged RAM and disk chunks.
const first = await source.checkpoint('./points/1.smolcheckpoint', { store: './points/store' });
const second = await source.checkpoint('./points/2.smolcheckpoint', { store: './points/store' });
await Machine.restoreCheckpoint('./points/2.smolcheckpoint', 'restored');
Machine.exportCheckpoint('./points/2.smolcheckpoint', './point-2.smolcheckpoint');
Machine.pruneCheckpointStore('./points/store');

// smol cloud — pass an API key, or set SMOL_CLOUD_TOKEN.
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

For a local VM whose published service starts through `exec()`, set
`waitForPorts: false` on creation. The SDK still waits for the guest agent,
so `exec()` is safe; call `waitUntilReady()` once the service is listening.
The default keeps waiting for all published ports. This option is local-only.

```ts
const m = await Machine.create(
  { image: 'node:24-alpine', ports: [{ host: 18080, guest: 8080 }], waitForPorts: false },
  { target: 'local' },
);
try {
  await m.exec(['sh', '-c',
    `nohup node -e 'require("http").createServer((_, res) => res.end("ok"))
      .listen(8080, "0.0.0.0")' >/tmp/server.log 2>&1 &`,
  ]);
  await m.waitUntilReady();
  console.log(await (await m.fetch(8080)).text());
} finally {
  await m.delete();
}
```

When reconnecting to clean up a local machine whose published service has
stopped, `Machine.connect(name, { target: 'local', waitForPorts: false })`
attaches after the agent is ready, so `delete()` can run without a listener.
The default continues waiting for all published ports.

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

Inside a branched rollout worker, `new RolloutClient()` discovers its authenticated
node assignment from `/etc/smolvm/branch-env` and automatically groups workers
from the same branch batch into a bounded cohort. Set `autoForkCohort: false` only
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

`await machine.pause()` saves execution durably and stops the VM;
`await machine.resume()` restores its processes, RAM and disks under the same identity.
Use a branchable machine. Unlike stop/start, resume does not boot a fresh guest.
Local saves need the machine's data directory; cloud saves use object storage.
Existing network connections may need to reconnect.

- `Machine.create(config?, conn?)` — create and start a machine; cloud waits for
  `ready === true` before returning.
- `Machine.connect(id, conn?)` — attach to an existing machine without waiting;
  call `waitUntilReady()` before use.
- `Machine.list(conn?, { labels? })` — every machine the target knows about,
  including ones other processes created, as `MachineSummary` rows.
- `machine.ready()` / `machine.readyAt()` /
  `machine.waitUntilReady({ timeoutMs, intervalMs })` *(cloud)*.
- `machine.exec(command, opts?)` / `machine.run(image, command, opts?)` → `ExecResult`.
- `machine.execStream(command, opts?)` → `AsyncGenerator<ExecEvent>`.
- `machine.readFile(path)` / `machine.writeFile(path, data, mode?)`.
- `machine.readFileStream(path)` yields `Uint8Array` chunks without buffering
  the full Cloud download; stopping iteration cancels the response. Local
  machines currently yield one buffered chunk.
- `machine.pullImage(image)` / `machine.listImages()`.
- `machine.branch(name, options?)` / `machine.branchBatch(options)`.
- `machine.checkpoint(output, { store? })`, `Machine.restoreCheckpoint(...)`,
  `Machine.exportCheckpoint(...)`, and `Machine.pruneCheckpointStore(...)`.
- `machine.resize({ cpus?, memoryMb?, storageGb?, overlayGb? })` *(local)* —
  add CPUs, RAM or disk to a running machine without rebooting it. Sizes are
  totals; returns the new `MachineResources`.
- `machine.stop()` / `machine.delete()` / `await machine.state()`. Cloud
  `"started"` means VM launched, not ready for work.

Errors are typed: `SmolError` (with `.code`), `ExecutionError`, `NotSupportedError`, `InvalidConfigError`.

### Machines that outlive the process (local)

A local machine normally dies with the process that started it: the engine
arms a parent-death watchdog, and the SDK stops the machines it owns on
SIGINT/SIGTERM. A long-running host service whose own restarts must not take
its machines down opts out with `detach`, labels its machines so it can tell
them from anyone else's, and reclaims them after a restart with `list` +
`connect`:

```ts
await Machine.create({ name: 'worker-1', detach: true, labels: { owner: 'hostd' } });
// … this process is killed and starts again …
for (const m of await Machine.list({}, { labels: { owner: 'hostd' } })) {
  const machine = await Machine.connect(m.name); // still running; no reboot
}
```

`detach` implies `persistent` and is remembered by the machine, so later
starts and branches are detached too. `smol machine ls` shows the same
machines. Nothing reaps a detached machine but `delete()`.

### One prepared checkpoint, a network policy per session (local)

Prepare an environment once with open egress, checkpoint it, and start every
session from it with its own network policy, applied before the session first
boots:

```ts
const prep = await Machine.create({
  image: 'node:22',
  branchable: true,
  resources: { network: true, networkBackend: 'virtio-net' },
});
await prep.exec(['npm', 'install', '-g', 'pnpm']);
await prep.checkpoint('prepared.smolcheckpoint');

const session = await Machine.restoreCheckpoint('prepared.smolcheckpoint', 'session-1', undefined, {
  networkPolicy: { allowHosts: ['registry.npmjs.org', '*.github.com'] }, // or 'deny-all' / 'allow-all'
});
```

- `allowHosts` entries are exact names or `*.` subdomain wildcards, which don't
  match the bare domain.
- A stopped machine's policy can be replaced with
  `machine.setNetworkPolicy(policy)`.
- The prepared machine must use `networkBackend: 'virtio-net'`, which enforces
  allow lists on the host. A restored machine keeps its checkpoint's backend, so
  an allow list on a TSI checkpoint is refused rather than left unenforced.

### API keys the workload uses without seeing

Bind a credential to the hosts it is for. The guest variable holds a
placeholder, and the real value is substituted only in the headers of HTTPS
requests to those hosts:

```ts
const machine = await Machine.create({
  image: 'alpine:3.20',
  credentials: [
    {
      name: 'notion',
      envVar: 'NOTION_API_KEY',
      hosts: ['api.notion.com', 'files.notion.com'],
      value: process.env.NOTION_API_KEY,
    },
  ],
});
await machine.exec(['sh', '-c', 'curl -H "Authorization: Bearer $NOTION_API_KEY" https://api.notion.com/v1/users/me']);
```

- Hosts are exact names; list each subdomain.
- Locally the value stays in this process's memory. Without `value`, this
  process's own `envVar` is read at each start, which is how a machine reopened
  by a later process gets it. `methods: ['GET', 'HEAD']` limits a read-only token.
- On the cloud, a credential with a `value` is stored for your account under
  `name`; one without refers to a credential already stored there.

## Cache disks (cloud)

A cache disk is a disk image many machines start from, each through its own
copy-on-write layer: dependencies, build caches, datasets. Publish a stopped
machine's cache as the next version, and later machines start from it.

```ts
import { CacheDisk, Machine } from "smolmachines";

await CacheDisk.create({ name: "deps" });                        // v0: empty
const m = await Machine.create({ image: "node:22", cacheDisk: { cache: "deps" } });
await m.exec(["sh", "-c", "cd /cache && npm install"]);
await m.stop();
await m.publishCacheDisk();                                       // v1
// Every machine created with cacheDisk: { cache: "deps" } now starts from v1.
```

One checkpoint can resume with many caches. Create the base machine with an
empty cache as a slot, which attaches it unmounted, and checkpoint it warm.
Each restore then mounts its own cache there, no larger than the slot and at
the slot's mount path:

```ts
const base = await Machine.create({ image: "node:22", cacheDisk: { cache: "slot-20g", slot: true } });
const { id } = await base.checkpoint();
const a = await Machine.restoreCheckpoint(id, "project-a", undefined, { cacheDisk: { cache: "project-a" } });
```

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
