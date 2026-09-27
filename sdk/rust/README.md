# smolmachines — Rust SDK

Run isolated **microVMs** from Rust, either locally or on
**smol cloud** — one `Machine` API, the target chosen by `ConnectOptions`.
Locally the SDK drives the installed `smolvm` CLI. Mirrors the [Node SDK](../node) and
[Python SDK](../python), which wrap the same engine through NAPI and pyo3.

> **Supported platforms** (local target): macOS on Apple Silicon, and Linux
> x64/arm64 with glibc ≥ 2.34, with a hypervisor — KVM on Linux, the Hypervisor
> framework on macOS. The **cloud** target works anywhere the crate builds.

```rust
use smolmachines::{Machine, Port};

let machine = Machine::builder("hello")
    .image("alpine:latest")
    .cpus(2)
    .memory_mib(1024)
    .network(true)
    .port(Port::new(8080, 80))
    .create()?;

machine.start()?;
let result = machine.exec(["uname", "-a"])?;
println!("{}", result.stdout_utf8());
machine.delete()?;
```

For a trusted local egress interceptor, pass its loopback address and token on
each launch. The SDK sends the token in the CLI child's environment, not its
command line, and keeps the binding on the current handle for later starts.
After reconnecting from another process, supply it again through
`ConnectOptions::egress_interceptor`.

```rust,no_run
# use smolmachines::{EgressInterceptor, Machine};
# fn main() -> smolmachines::Result<()> {
let binding = EgressInterceptor::new(
    "127.0.0.1:9000".parse().unwrap(),
    std::env::var("SMOLVM_INTERCEPTOR_TOKEN").unwrap(),
);
let machine = Machine::builder("protected").network(true).create()?;
machine.start_with_interceptor(&binding)?;
# Ok(())
# }
```

## TCP tunnels

Use `machine.tunnel(22)?` to reach a published port through a scoped loopback
listener, locally or over an authenticated cloud WebSocket:

Local tunnels use the port mappings supplied when this SDK handle created the
machine; reconnecting by name cannot recover mappings from older CLIs.

```rust,no_run
# use smolmachines::Machine;
# fn connect(machine: &Machine) -> smolmachines::Result<()> {
let tunnel = machine.tunnel(22)?;
println!("Connect your SSH client to {}", tunnel.address());
// Keep the handle alive while clients use it.
drop(tunnel); // Disconnects clients, not the machine.
# Ok(())
# }
```

For cloud services you start through exec, set `.wait_for_ports(false)` on the
builder. Creation then waits for guest exec readiness, not the published service.
The default still waits for service readiness. Local creation still requires
`machine.start()`.

## Local or cloud

### Managed agents on smol cloud

`CloudAgentSession` drives the hosted `/v1/agents` lifecycle, including background turns, replayable events, cancellation, rewind, and branch. This is separate from the local `agent::Session` API.

```rust
use smolmachines::{cloud_agent::CloudAgentSession, smol_cloud::types::{CreateAgent, SendAgentTurn}, ConnectOptions};

let agent = CloudAgentSession::create(&CreateAgent {
    name: "fixer".into(),
    harness: Some("claude-code".into()),
    credential: Some("anthropic".into()),
    ..Default::default()
}, &ConnectOptions::cloud())?;
while agent.info()?.status == "starting" {
    std::thread::sleep(std::time::Duration::from_secs(2));
}
let turn = agent.send(&SendAgentTurn {
    prompt: "Fix the failing tests".into(),
    env: Default::default(),
    timeout_seconds: None,
}, Some("task-123"))?;
for event in agent.events(turn, None)? { println!("{:?}", event?); }
```

Wait until `agent.info()?.status == "ready"` before sending. The application still stages its repository in `/workspace` through the machine API.

The default is local. Only an **explicit** credential moves that — an `api_key`
or `SMOL_CLOUD_TOKEN` — so a `smol auth login` session on disk never silently
redirects a program that meant to run locally.

```rust
use smolmachines::{ConnectOptions, Machine};

// Local: the SDK drives smolvm on this host.
let local = Machine::builder("here").image("alpine:latest").create()?;

// Cloud: the control plane runs it. create_with also starts the machine and
// waits for its agent, so it is ready to work when this returns.
let remote = Machine::builder("there")
    .image("alpine:latest")
    .auto_stop_seconds(300)
    .create_with(&ConnectOptions::cloud())?;

println!("{}", remote.exec(["uname", "-a"])?.stdout_utf8());
println!("{} µ$ so far", remote.usage()?.cost.total_micros);
```

smol cloud runs both **arm64 and amd64**, and `.arch()` picks one. Leaving it
unset lets the control plane place the machine wherever it has room, which is
usually what you want — set it when something downstream cares:

```rust
let machine = Machine::builder("on-arm")
    .image("alpine:latest")
    .arch("arm64")
    .create_with(&ConnectOptions::cloud())?;
```

Publishing a port changes when a cloud machine is considered ready: readiness
then waits for something to accept a connection on it. Publish a port nothing
serves and the machine never becomes ready, so prefer no ports on a machine you
only exec into.

A checkpoint is the usual reason to care: a capture only restores on the
architecture it was taken on, and `captured.cloud().arch` reports which that
was. Locally there is only the host's architecture, so asking for a different
one is an error rather than a silent no-op.

The two targets are not one machine at a different address, and the SDK does not
pretend otherwise:

| | local | cloud |
|---|---|---|
| `exec`, `exec_stream`, files, `branch`, `checkpoint` | ✅ | ✅ |
| host mounts, `run(image, …)`, `pull_image`, `list_images`, `sync` | ✅ | ❌ |
| `usage`, `delete_with_usage`, `share`, `unshare`, `checkpoints`, `list_cloud_machines` | ❌ | ✅ |
| `pid` | the VM's child process | `None` — it runs on someone else's node |

Asking for the wrong one returns `ErrorKind::NotSupported` naming the target it
needs, rather than failing obscurely later. Two config mistakes are caught up
front for the same reason: a cloud machine needs an image (there is no local
rootfs to fall back on), and cannot take host bind-mounts (there is no host
filesystem to bind), so a config carrying either is rejected instead of quietly
producing a machine missing its data.

`checkpoint` returns a `Checkpoint` enum, because a capture is a different
object on each target: locally a file you chose the path for, whose interesting
part is what it cost; on the cloud a durable object the control plane holds,
whose interesting part is how to get it back.

## The local engine

*Local target only — a cloud machine runs on someone else's node.*

Local machines are run by a `smolvm` engine process, not by linking the engine
into your own. That is deliberate: crates.io resolves every dependency, and the
engine crate is not published, so linking it would make this SDK impossible to
publish. Fetching the released engine instead is what npm and PyPI do for their
SDKs — they ship per-platform binaries — and it keeps `cargo add smolmachines`
sufficient on its own.

If you ship your own engine assets, point them out once before the first
machine:

```rust
use smolmachines::{configure_runtime_assets, RuntimeAssets};

// Borrow an installed smolvm of this SDK's version, if one is on PATH.
if let Some(assets) = RuntimeAssets::from_path_lookup() {
    configure_runtime_assets(assets)?;
}
```

A program that ships its own engine names the paths itself. See
[Runtime assets](#runtime-assets).

## Install

```sh
cargo add smolmachines
```

That is the whole install. The crate links no engine and carries no binaries,
so it builds anywhere Rust does.

**Cloud machines need nothing else.** For **local** machines the SDK needs an
engine, and finds one in this order: `SMOLVM`, then `PATH` and the usual
install locations, and failing those it fetches the matching engine release
once and caches it (~37 MiB, checksum-verified against the release's published
`checksums.sha256`). Nothing is downloaded unless you actually ask for a local
machine, and the engine is pinned to this crate's own version so the two cannot
drift.

| variable | effect |
|---|---|
| `SMOLVM` | use this binary and look no further |
| `SMOLVM_BOOT_BINARY` | ignored on purpose — it ties a VM's life to its parent, and every CLI call here is short-lived |
| `SMOLMACHINES_CACHE_DIR` | where fetched engines live (default: your cache dir) |
| `SMOLMACHINES_ENGINE_VERSION` | fetch a different engine version |
| `SMOLMACHINES_NO_DOWNLOAD=1` | never fetch; fail instead |

The control-plane wire types and client live in
[`smol-cloud`](https://crates.io/crates/smol-cloud), shared with the `smol` CLI
so the two clients of one API cannot drift. It is re-exported as
`smolmachines::smol_cloud` if you need the raw API.

## Branching

A machine created `branchable` and started with `start_branchable` can be
branched. A branch shares the source's live guest RAM and disks
copy-on-write, so it costs a fraction of a boot and starts from the exact state
the source was in, warm caches included.

```rust
use smolmachines::{BranchOptions, Machine};

let source = Machine::builder("source")
    .image("alpine:latest")
    .branchable(true)
    .create()?;
source.start_branchable()?;
source.exec(["sh", "-c", "echo warm > /tmp/state"])?;

// One branch.
let child = source.branch("child")?;

// Or many, booted in bounded waves. Transactional: if one fails, none survive.
let names: Vec<String> = (0..16).map(|i| format!("worker-{i}")).collect();
let workers = source.branch_batch(names, BranchOptions::new().parallel(8))?;
```

## Checkpoints

For a managed save under the same machine identity, use `machine.pause()?` and
`machine.resume()?`. Pause waits for durable storage before stopping; resume
restores processes, RAM and disks rather than booting a fresh guest. Use a
branchable machine. Local saves need the machine's data directory; cloud saves
use object storage. Existing network connections may need to reconnect.

A checkpoint is a portable capture of a running machine. Point every capture of
a machine at the same store and all but the first are incremental: unchanged
chunks are reused rather than written again, which is what makes a warm capture
cheap enough to take often.

```rust
use smolmachines::{CheckpointOptions, Machine};
use std::path::Path;

let captured = machine.checkpoint_with(
    Some(Path::new("state.smolcheckpoint")),
    CheckpointOptions::new().store_dir("/var/lib/checkpoints"),
)?;
let result = captured.local().expect("a local machine captures locally");
println!("{} MiB written, {} MiB reused", result.size_bytes >> 20, result.reused_bytes >> 20);

let restored = Machine::restore_checkpoint("restored", "state.smolcheckpoint")?;
restored.start()?;
```

`result.source_pause` is the only part the workload notices; `result.elapsed`
covers the whole capture.

On the cloud, pass `None` instead: the control plane stores the capture and
`captured.cloud()` carries its id, size and download URL. Restoring goes by that
id, not a path — the artifact never touches your disk:

```rust
let captured = machine.checkpoint(None)?;
let stored = captured.cloud().expect("a cloud machine captures to the cloud");

let restored = Machine::restore_cloud_checkpoint("revived", &stored.id, &ConnectOptions::cloud())?;
println!("{}", restored.exec(["cat", "/tmp/note"])?.stdout_utf8());
```

Unlike the local restore, this starts the machine and waits for it, and deletes
it if it cannot become ready — a cloud machine that exists but never came up is
an orphan that bills.

## Streaming output

`exec` waits for the command and hands back everything it wrote. When output
matters as it arrives, `exec_stream` yields each chunk instead, as a plain
iterator:

```rust
use smolmachines::{ExecEvent, ExecOptions};

for event in machine.exec_stream(["sh", "-c", "make 2>&1"], ExecOptions::new()) {
    match event {
        ExecEvent::Stdout(chunk) => print!("{}", String::from_utf8_lossy(&chunk)),
        ExecEvent::Stderr(chunk) => eprint!("{}", String::from_utf8_lossy(&chunk)),
        ExecEvent::Exit(code) => println!("exited {code}"),
        ExecEvent::Error(message) => eprintln!("stream failed: {message}"),
    }
}
```

Dropping the stream early leaves the command running in the guest. It does not
kill it.

## Mounts

A local source is bind-mounted from the host. An `s3://` source is fetched by
the in-guest agent instead, and is routed to the engine's remote volumes
automatically, so both are declared the same way:

```rust
use smolmachines::Mount;

Machine::builder("build")
    .mount(Mount::new("/home/me/project", "/workspace"))
    .mount(Mount::new("/usr/share/data", "/data").read_only())
    .mount(Mount::new("s3://bucket/models", "/models").read_only())
    // Copied into guest-local storage at start; `sync()` copies changes back.
    .mount(Mount::new("/home/me/cache", "/cache").staged())
    .create()?;
```

## Blocking

Every call blocks. The engine is synchronous, so an async caller should run
these on a blocking pool with `tokio::task::spawn_blocking` or equivalent. That
is exactly what the Node and Python SDKs do internally; here the choice is left
to the caller rather than made for them.

## Errors

Every failure is a `smolmachines::Error` carrying an `ErrorKind` and the
underlying message. The kinds and their string codes match the Node and Python
SDKs, so the three agree on what a failure is called. A cloud error also keeps
the server's `x-request-id` in its message — the caller never sees response
headers, and support needs that id to find the call:

```rust
use smolmachines::ErrorKind;

match machine.start() {
    Err(error) if error.kind() == ErrorKind::KvmUnavailable => {
        eprintln!("this host has no hypervisor: {}", error.message());
    }
    other => other?,
}
```

## Runtime assets

`RuntimeAssets::from_path_lookup` covers the common case of an installed
smolvm. A program that ships its own boot binary, hypervisor libraries or guest
rootfs names the paths itself. Anything already set in the environment wins, so
an operator can override a build-time path without a recompile:

```rust
use smolmachines::{configure_runtime_assets, RuntimeAssets};

configure_runtime_assets(
    RuntimeAssets::new()
        .boot_binary("/opt/app/smolvm-boot")
        .lib_dir("/opt/app/lib")
        .agent_rootfs("/opt/app/rootfs"),
)?;
```

## Examples

```sh
cargo run --example hello       # boot, exec, tear down
cargo run --example fanout      # branch one warm machine into eight
cargo run --example checkpoint  # cold capture, warm capture, restore
cargo run --example cloud       # the same API against smol cloud
cargo run --example cloud_checkpoint  # capture a cloud machine and bring it back
cargo run --example e2e_local   # walk every local operation and report on each
cargo run --example e2e_cloud   # the same walk against smol cloud (creates billable machines)
```
