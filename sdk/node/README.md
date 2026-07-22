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

// Cloud (smolfleet) — pass an API key, or set SMOL_CLOUD_TOKEN (e.g. via `smol login`):
const cloud = await Machine.create(
  { image: 'python:3.12' },
  { target: 'cloud', apiKey: process.env.SMOL_CLOUD_TOKEN },
);
const res = await cloud.exec(['python', '-c', 'print(40 + 2)']);
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
const m = await Machine.create({ image, ports: [{ host: 8080, guest: 8080 }] },
                               { target: 'cloud' });
await m.waitUntilReady();          // create() already waited; re-assert if reconnecting
console.log(await m.ready(), await m.readyAt());
```

To reach a service **inside** the VM, use the authenticated connect bridge —
**no Cloudflare/localhost.run tunnel, no public exposure, no egress allow-list.**
Have the worker LISTEN on a published port and connect *inbound*:

```ts
// HTTP to a published guest port:
const res = await m.fetch(8080, '/healthz');

// Or a WebSocket, using your own ws client with the authed endpoint:
const { wsUrl, headers } = m.endpoint(8080, '/socket');
const ws = new WebSocket(wsUrl, { headers });   // e.g. the `ws` package
```

## Install

```bash
npm install smolmachines
```

Requires Node.js ≥ 18 on a host the engine supports (macOS Apple Silicon, or Linux
with KVM).

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

- `Machine.create(config?, conn?)` — create and start a machine.
- `machine.exec(command, opts?)` / `machine.run(image, command, opts?)` → `ExecResult`.
- `machine.execStream(command, opts?)` → `AsyncGenerator<ExecEvent>`.
- `machine.readFile(path)` / `machine.writeFile(path, data, mode?)`.
- `machine.pullImage(image)` / `machine.listImages()`.
- `machine.stop()` / `machine.delete()` / `await machine.state()` → `"running" | "stopped"`.

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
