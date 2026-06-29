<p align="center">
  <img src="assets/logo.png" alt="smol machines" width="80">
</p>

<p align="center">
  <a href="https://github.com/smol-machines/smol/releases"><img src="https://img.shields.io/github/v/release/smol-machines/smol?label=CLI" alt="CLI release"></a>
  <a href="https://github.com/smol-machines/smol/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License"></a>
  <a href="https://www.npmjs.com/package/smolmachines"><img src="https://img.shields.io/npm/v/smolmachines?label=npm" alt="npm"></a>
  <a href="https://pypi.org/project/smolmachines/"><img src="https://img.shields.io/pypi/v/smolmachines?label=PyPI" alt="PyPI"></a>
</p>

# smol

Run code in isolated **microVM sandboxes** — from the command line, or embedded
directly in your **Node** or **Python** app. The same `Machine` API works
locally (an in-process microVM via the bundled smolvm engine — no server) or
against the **smolfleet** cloud, selected at connect time.

```
┌──────────────┐     ┌──────────────────────────── one Machine API ┐
│   smol CLI   │     │  LocalTransport  ──▶ embedded smolvm microVM  │
│  (Rust)      │     │  CloudTransport  ──▶ smolfleet /v1 (REST)     │
└──────────────┘     └──────────────────────────────────────────────┘
```

## Components

| Path | What |
|------|------|
| `sdk/node` | Node SDK — NAPI native core + TypeScript. Local (embedded) or cloud. |
| `sdk/python` | Python SDK — pyo3 native core + pure-Python layer. Same API. |
| `src/` | The `smol` CLI (Rust): create / run / exec / files / logs, plus cloud deploy + a container registry. |
| `docs/cli.md` | CLI command reference. |

## Quickstart — Node

```ts
import { Machine } from 'smolmachines';

// Local: boots an in-process microVM, no server.
const m = await Machine.create({ resources: { cpus: 2, memoryMb: 1024, network: true } });
try {
  const r = await m.run('python:3.12', ['python', '-c', 'print(2 ** 10)']);
  console.log(r.stdout);            // 1024
  await m.writeFile('/tmp/in.txt', 'hi');
  console.log((await m.readFile('/tmp/in.txt')).toString());
} finally {
  await m.delete();
}

// Cloud: same API, just point at smolfleet.
const c = await Machine.create(
  { image: 'alpine:3.20' },
  { target: 'cloud', apiKey: process.env.SMOL_CLOUD_TOKEN },
);
```

## Quickstart — Python

```python
from smol import Machine, MachineConfig, ResourceSpec, ConnectOptions

# Local (embedded microVM) — context-managed, auto-deletes on exit.
with Machine.create(MachineConfig(resources=ResourceSpec(cpus=2, memory_mb=1024, network=True))) as m:
    r = m.run("python:3.12", ["python", "-c", "print(2 ** 10)"]).assert_success()
    print(r.stdout)                  # 1024

# Cloud — same API.
with Machine.create(
    MachineConfig(image="alpine:3.20"),
    ConnectOptions(target="cloud", api_key="smk_…"),  # or set SMOL_CLOUD_TOKEN
) as m:
    print(m.exec(["echo", "hi"]).stdout)
```

## Quickstart — CLI

```bash
smol run python:3.12 -- python -c "print(2**10)"   # ephemeral one-shot
smol create mybox --image alpine:3.20              # persistent machine
smol exec --name mybox -- apk add curl
smol ls
smol rm mybox

# cloud (smolfleet)
smol login
smol deploy --image alpine:3.20
smol machines
```

See **[docs/cli.md](docs/cli.md)** for the full command reference, and run
`smol <command> --help` for flags.

## Install

### SDK

```bash
npm install smolmachines      # Node
pip install smolmachines      # Python
```

Prebuilt packages bundle everything the **local** transport needs — the
`libkrun` libraries, a code-signed boot helper, and the guest rootfs — so
`Machine.create()` boots an in-process microVM with no separate install (macOS
Apple Silicon and Linux x86_64/arm64, glibc ≥ 2.34). The **cloud** transport is
pure-language and runs anywhere. Booting a local microVM needs hardware
virtualization (macOS Hypervisor.framework or Linux `/dev/kvm`).

### CLI

The standalone **`smol` CLI** (create / run / exec, pack `.smolmachine`
artifacts, container registry, cloud deploy) installs with a self-contained
bundle — no SDK or separate engine required:

```bash
curl -sSL https://raw.githubusercontent.com/smol-machines/smol/main/scripts/install.sh | bash
```

The script auto-detects your platform, downloads the matching release bundle
(which carries its own `libkrun` runtime + guest agent), verifies its checksum,
extracts it to `~/.smol`, and symlinks `smol` onto your `PATH` (`~/.local/bin`).
Pin a release with `SMOL_VERSION=v1.3.2`; override locations with `PREFIX` /
`BIN_DIR`. Supports macOS Apple Silicon and Linux x86_64/arm64.

## Building from source / the engine core

This repository is the open **`smol` SDK + CLI** (Apache-2.0). The microVM
**engine** it links — `smolvm`, which wraps `libkrun` — is also open source
(Apache-2.0), in its own repository:
[**smol-machines/smolvm**](https://github.com/smol-machines/smolvm). The
published packages bundle prebuilt engine binaries so you don't have to build it
yourself.

The Rust crates here (`src/` CLI and the `sdk/*/src` native cores) declare a
**path dependency** on the engine (`smolvm = { path = ".." }`), so a native
build needs the `smolvm` repo checked out alongside this one — a checkout of
**just this repo does not build standalone**. You can still read and modify the
SDK/CLI source, the TypeScript/Python layers, tests, and docs without it; full
native builds are produced by maintainers' CI. Most contributions don't need a
native build — see [CONTRIBUTING.md](CONTRIBUTING.md). To report a security
issue, see [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE).
