# smol — Agent Reference

`smol` runs code in isolated microVM sandboxes, from a CLI or embedded in a Node
or Python app. The same `Machine` API drives both **local** microVMs (in-process,
via the bundled smolvm engine — no server) and the **smolfleet cloud**; the
transport is chosen at connect time, not by using a different API.

```
┌──────────────┐     ┌──────────────────────────── one Machine API ┐
│   smol CLI   │     │  LocalTransport  ──▶ embedded smolvm microVM  │
│   (Rust)     │     │  CloudTransport  ──▶ smolfleet /v1 (REST)     │
└──────────────┘     └──────────────────────────────────────────────┘
```

## Platform Support

| | Local (embedded microVM) | Cloud (smolfleet) |
|---|---|---|
| macOS Apple Silicon | ✅ Hypervisor.framework | ✅ |
| Linux x86_64 / arm64 | ✅ needs `/dev/kvm`, glibc ≥ 2.34 | ✅ |
| Anything else | ❌ | ✅ pure-language, runs anywhere |

Prebuilt SDK packages bundle everything the local transport needs (the `libkrun`
libraries, a code-signed boot helper, the guest rootfs), so `Machine.create()`
boots a microVM with no separate install. The cloud transport needs only a token.

## Quick Reference

### Node

```ts
import { Machine } from 'smolmachines';

// Local — boots an in-process microVM, no server.
const m = await Machine.create({ resources: { cpus: 2, memoryMb: 1024, network: true } });
try {
  const r = await m.run('python:3.12', ['python', '-c', 'print(2 ** 10)']);
  await m.writeFile('/workspace/in.txt', 'hi');
} finally {
  await m.delete();
}

// Cloud — same API, different target.
const c = await Machine.create({ image: 'alpine:3.20' }, { target: 'cloud' });
```

### Python

```python
from smol import Machine, MachineConfig, ResourceSpec, ConnectOptions

# Context-managed: auto-deletes on exit.
with Machine.create(MachineConfig(resources=ResourceSpec(cpus=2, memory_mb=1024, network=True))) as m:
    r = m.run("python:3.12", ["python", "-c", "print(2 ** 10)"]).assert_success()

m = Machine.create(MachineConfig(image="alpine:3.20"), ConnectOptions(target="cloud"))
```

### CLI

```bash
smol run python:3.12 -- python -c "print(2**10)"   # ephemeral one-shot
smol machine create mybox --image alpine:3.20      # persistent machine
smol machine exec --name mybox -- apk add curl
smol machine ls                                    # local + cloud
smol machine rm mybox

smol auth login                                    # cloud
smol cloud deploy --image alpine:3.20
smol machine ls --cloud
```

## When to Use What

| Goal | Use |
|------|-----|
| One-off command, no state | `smol run IMAGE -- CMD`, or SDK `machine.run(image, cmd)` |
| Sandbox inside your app | SDK `Machine.create()` — local target |
| Same code, someone else's hardware | SDK `Machine.create(cfg, { target: 'cloud' })` |
| Long-lived named environment | `smol machine create` → `exec` |
| Ship software as one binary | `smol pack create` |
| Many sandboxes from one warm state | `machine.branch()` / `branchBatch()` |
| RL rollouts with reward attribution | `machine.assign()` → `Episode` → `rewardFork()` |

## Machine API

Both SDKs expose the same surface (camelCase in Node, snake_case in Python).

| Group | Methods |
|-------|---------|
| Lifecycle | `create`, `connect`, `start`, `stop`, `delete` |
| Readiness | `state`, `ready`, `readyAt`, `waitUntilReady` |
| Execute | `exec`, `run`, `execStream` (Node: async generator) |
| Files | `readFile`, `writeFile` |
| Images | `pullImage`, `listImages` |
| Networking | `endpoint`, `url`, `fetch` (Node) / `request` (Python) |
| Branch | `branch`, `branchBatch` (`fork`/`forkBatch` compatibility aliases) |
| Rollouts | `assign` → `Episode` (`exec`, `heartbeat`, `complete`, `status`), `rewardFork` |

🔴 **`create()` waits until the guest agent is reachable.** A *cloud* machine
reporting state `started` only means the VM launched — not that it can accept
work. Use `create()`/`waitUntilReady()` rather than polling for `started`.

🔴 **Write files where they survive: `/workspace`, not `/tmp`.** `/workspace`
and the image filesystem are on the machine's storage disk and persist across
`exec` calls and stop/start. `/tmp`, `/run`, and `/dev/shm` are in memory: they
keep their contents while the machine runs, but are empty after it stops and
starts — including an automatic idle stop. Keep credentials, configuration, and
any state a later command reads out of `/tmp`, and do not point a config path at
a symlink into `/tmp`.

## CLI Structure

Full reference in [`docs/cli.md`](docs/cli.md); `smol <command> --help` for flags.

| Group | What |
|-------|------|
| `smol run` | Ephemeral one-shot |
| `smol machine …` | Lifecycle: create, start, exec, ls, rm, branch, logs, cp |
| `smol file …` | Declarative Smolfile workflows |
| `smol pack …` | Build and run `.smolmachine` artifacts |
| `smol registry …` | Container registry: login, push, pull |
| `smol auth …` | Cloud authentication |
| `smol cloud …` | Deploy, exec, scale, export against smolfleet |
| `smol rollout …` | Fused policy rollouts (LoRA versions on a CUDA node) |
| `smol config …` | Local configuration |

## Repo Layout

| Path | What |
|------|------|
| `src/` | The `smol` CLI (Rust). One file per command in `src/commands/`. |
| `sdk/node` | Node SDK — NAPI native core (`sdk/node/src/*.rs`) + TypeScript (`machine.ts`, `index.ts`). |
| `sdk/python` | Python SDK — pyo3 native core (`sdk/python/src/*.rs`) + pure Python (`sdk/python/python/smol/`). |
| `docs/cli.md` | CLI command reference. |

Both SDK native cores and the CLI path-depend on the **smolvm engine** in the
sibling `smol-machines/smolvm` repo, so a local build needs it checked out
alongside this one. CI checks it out automatically.

## Environment Variables

| Variable | Effect |
|----------|--------|
| `SMOL_CLOUD_TOKEN` | Cloud auth token. Set by `smol auth login`; read by both SDKs. |
| `SMOL_CLOUD_URL` / `SMOL_CLOUD_API` | Point the cloud transport at a different control plane. |
| `SMOL_LOG` | Log filter for the CLI and SDKs. |
| `SMOL_FORK_DEBUG` | Extra diagnostics on the fork path. |

Install script (`scripts/install.sh`): `SMOL_VERSION` pins a release, `PREFIX`
and `BIN_DIR` override install locations.
