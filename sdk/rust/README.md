# smolmachines — Rust SDK

Embed isolated **microVMs** directly in a Rust program. No daemon, no socket:
the engine is a library in your process and the VM is its child. Mirrors the
[Node SDK](../node) and [Python SDK](../python), which wrap this same engine
through NAPI and pyo3.

> **Supported platforms**: macOS on Apple Silicon, and Linux x64/arm64 with
> glibc ≥ 2.34. The host needs a hypervisor: KVM on Linux, the Hypervisor
> framework on macOS.

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

## The boot helper

To boot a VM the engine re-executes a binary that knows how to be a VM. In a
CLI that binary is the CLI itself; in an embedded process it is your program,
which does not, so the boot fails with a bare non-zero exit. Point the engine
at a real helper once, before the first machine:

```rust
use smolmachines::{configure_runtime_assets, RuntimeAssets};

// Borrow an installed smolvm, if there is one on PATH.
if let Some(assets) = RuntimeAssets::from_path_lookup() {
    configure_runtime_assets(assets)?;
}
```

A program that ships its own engine names the paths itself. See
[Runtime assets](#runtime-assets).

## Install

The crate path-depends on the `smolvm` engine checked out beside `smol/`, the
same layout the Node and Python SDKs use:

```toml
[dependencies]
smolmachines = { path = "../smol/sdk/rust" }
```

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

A checkpoint is a portable capture of a running machine. Point every capture of
a machine at the same store and all but the first are incremental: unchanged
chunks are reused rather than written again, which is what makes a warm capture
cheap enough to take often.

```rust
use smolmachines::{CheckpointOptions, Machine};

let result = machine.checkpoint_with(
    "state.smolcheckpoint",
    CheckpointOptions::new().store_dir("/var/lib/checkpoints"),
)?;
println!("{} MiB written, {} MiB reused", result.size_bytes >> 20, result.reused_bytes >> 20);

let restored = Machine::restore_checkpoint("restored", "state.smolcheckpoint")?;
restored.start()?;
```

`result.source_pause` is the only part the workload notices; `result.elapsed`
covers the whole capture.

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
engine's own message. The kinds and their string codes match the Node and
Python SDKs, so the three agree on what a failure is called:

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
```
