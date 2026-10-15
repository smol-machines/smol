# smol (Go SDK)

Embed isolated microVMs directly in your Go program. The same `Machine` API
drives local microVMs, booted in-process by the embedded smolvm engine with no
server to run, and machines on Smol Cloud.

```go
import smol "github.com/smol-machines/smol/sdk/go"

m, err := smol.Create(ctx, smol.MachineConfig{
	Resources: smol.ResourceSpec{CPUs: 2, MemoryMB: 1024, Network: smol.Bool(true)},
}, nil)
if err != nil {
	return err
}
defer m.Delete(context.Background())

r, err := m.Run(ctx, "python:3.12-alpine", []string{"python", "-c", "print(2 ** 10)"}, nil)
fmt.Print(r.Stdout) // 1024
```

Cloud is the same code with a different target:

```go
m, err := smol.Create(ctx, smol.MachineConfig{Image: "alpine:3.20"},
	&smol.ConnectOptions{Target: smol.TargetCloud})
```

## Install

```bash
go get github.com/smol-machines/smol/sdk/go
```

Requires Go 1.25 or newer. Local machines need macOS on Apple Silicon, or
Linux x86_64/arm64 with KVM and glibc 2.34 or newer. The cloud target works on
any platform Go builds for.

The package needs no cgo: it loads the engine with
[purego](https://github.com/ebitengine/purego), so `CGO_ENABLED=0` builds and
cross-compiles work.

### The local runtime

A Go module cannot carry per-platform binaries, so on first local use the SDK
downloads the runtime released with its version: the engine library, a
code-signed boot helper, the `libkrun` hypervisor libraries and the guest
rootfs (about 50 MB). It checks the download against the release's
`go-runtime-checksums.sha256` and caches it, so later runs start offline. This
is the same runtime the npm and PyPI packages ship.

| Variable | Effect |
|----------|--------|
| `SMOLMACHINES_CACHE_DIR` | Cache location (default `~/Library/Caches/smolmachines` or `~/.cache/smolmachines`), shared with the Rust SDK. |
| `SMOLMACHINES_NO_DOWNLOAD=1` | Fail instead of downloading, for hosts that may not reach the network at run time. |
| `SMOLMACHINES_RUNTIME_DIR` | Use an unpacked runtime bundle instead, for air-gapped hosts and local builds. |

`smol.CheckLocal()` reports whether this host can run local machines, and the
error code a local `Create` would fail with if not, without booting one.

The VM itself never runs inside your process. The engine launches each machine
in a separate `smol-vmm` helper, confined by seccomp and Landlock on Linux.

## Usage

```go
// Commands. Ending ctx kills the command and returns ctx's error.
r, err := m.Exec(ctx, []string{"sh", "-c", "echo $GREETING"},
	&smol.ExecOptions{Env: map[string]string{"GREETING": "hi"}})
if err := r.AssertSuccess(); err != nil { ... } // *smol.ExecutionError

// Streaming output. Leaving the loop early kills the command.
for ev, err := range m.ExecStream(ctx, []string{"make", "test"}, nil) {
	if err != nil {
		return err
	}
	if ev.Kind == smol.EventStdout {
		os.Stdout.Write(ev.Data)
	}
}

// Files.
err = m.WriteFile(ctx, "/workspace/in.txt", []byte("hello"), 0o644)
data, err := m.ReadFile(ctx, "/workspace/in.txt")

// Ports: publish a guest port on a running machine, then reach it.
p, err := m.Publish(ctx, smol.PublishOptions{Guest: 8080}) // free host port picked
res, err := m.Fetch(ctx, 8080, "/health")
err = m.Unpublish(ctx, smol.UnpublishOptions{Guest: 8080})

// Jobs (cloud): run detached, read output later, wait or cancel.
job, err := m.StartJob(ctx, []string{"make", "test"}, smol.JobOptions{Timeout: time.Hour})
final, err := job.Wait(ctx, smol.JobWaitOptions{})
out, err := job.ReadAll(ctx, "stdout")

// Branching: copy-on-write children with the source's warm RAM and disk.
src, err := smol.Create(ctx, smol.MachineConfig{Branchable: true}, nil)
child, err := src.Branch(ctx, "child-1", smol.BranchOptions{})
kids, err := src.BranchBatch(ctx, smol.BranchBatchOptions{Count: 8})

// Checkpoints.
info, err := src.Checkpoint(ctx, "./state.smolcheckpoint", smol.CheckpointOptions{})
restored, err := smol.RestoreCheckpoint(ctx, "./state.smolcheckpoint", "restored", nil,
	smol.RestoreCheckpointOptions{})
```

Errors are `*smol.Error` values with a stable `Code`, the same codes the Node
and Python SDKs use. Match them with `errors.Is`:

```go
if errors.Is(err, smol.ErrNotFound) { ... }
if errors.Is(err, smol.ErrNotSupported) { ... } // e.g. Usage() on the local target
```

### Choosing the target

`ConnectOptions.Target` picks `smol.TargetLocal` or `smol.TargetCloud`. Left
unset, the SDK uses the cloud when an API key is given (`ConnectOptions.APIKey`
or `SMOL_CLOUD_TOKEN`), and the local engine otherwise. A library embedding
the SDK should set the target explicitly. On the cloud target the key falls
back to the session `smol auth login` stored.

### Machines that outlive the process (local)

A local machine is tied to the process that made it: the engine reaps its VM
moments after the process dies, and the SDK stops it on SIGINT or SIGTERM
before letting the signal through. Set `Detach` for a machine that should
survive, and pick it up later with `smol.Connect(ctx, name, nil)`. `Labels`
and `smol.List` help a restarted process find its own machines. Set
`ConnectOptions.NoSignalHandling` when your program owns shutdown.

### Blocking calls

Local engine calls block their goroutine until the engine answers; run them
concurrently with goroutines as you would any blocking I/O. `ctx` is checked
before each call, and only `Exec` and `ExecStream` are cut short when it ends.

## Building from source

The engine binding is a small Rust library in `native/` that path-depends on
the smolvm engine, so the smol checkout must sit inside a smolvm checkout
(`<engine>/smol/sdk/go/native`).

```bash
cd native && cargo build --release          # → target/release/libsmol_go.{dylib,so}
```

To run the SDK against that build, unpack a released runtime bundle, replace
its `libsmol_go` with yours, and point `SMOLMACHINES_RUNTIME_DIR` at the
directory. `scripts/bundle-runtime.sh` builds a bundle from the npm platform
package's `native/` directory.

```bash
go test ./...                                  # unit + cloud-mock tests
SMOL_E2E=1 SMOLMACHINES_RUNTIME_DIR=… go test -run E2E ./...   # boots real machines
```

## License

Apache-2.0
