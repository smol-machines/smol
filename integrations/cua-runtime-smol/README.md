# cua-runtime-smol

Run [Cua](https://cua.ai) Spaces as smol machines.

Cua gives agents desktops they can use. A Space runs on a local runtime, and
Cua lets a runtime be installed separately: an executable named
`cua-runtime-<name>` that answers a small JSON protocol. This is that
executable for smol machines. Once it is on your `PATH`, Cua's Linux desktop
image boots as a smol machine, and everything Cua does with a Space works:

```bash
cua runtime doctor                                            # smol    ok    smol machines (microvm)
cua sb create ghcr.io/trycua/linux:24.04 --runtime smol --name dev
cua sb exec dev -- 'hostname'
cua sb fork dev dev2                                          # a live branch: 0.4 s, memory and all
cua sb checkpoint dev snap                                    # a frozen branch base: under a second
cua sb fork snap dev3                                         # 0.3 s, from the checkpoint's memory
cua sb view dev                                               # the desktop, in the browser
cua sb rm -f dev dev2 dev3
```

The same from the Cua SDK: `Sandbox.create(image, runtime="smol")`.

Measured on a Linux workstation with the image already pulled: create 2.3 s,
fork 0.43 s, checkpoint 0.88 s, fork of a checkpoint 0.25 s, suspend 1.4 s,
resume 2.2 s, exec 0.25 s. The first create pulls the image (a minute or two).

## What you get that the other runtimes do not

- **fork is a live branch.** `cua sb fork` of a running Space clones its
  memory, processes and open connections copy-on-write. The source never
  pauses and the clone is up in well under a second regardless of how big the
  Space is. Forks of forks work the same way, and every fork is a new machine:
  its own hostname, machine id and random state.
- **checkpoint is a frozen branch, so forking it is instant.** A checkpoint
  is a branch of the Space frozen by the engine as a branch base: the memory
  stays shared, nothing is written out, and every `fork` of it is another
  branch. Set `CUA_RUNTIME_SMOL_CHECKPOINT_FILES=1` to also write a portable
  `.smolcheckpoint` file under `~/.smol/cua-runtime/checkpoints/`, the
  Space's memory and disks as one file you can keep, copy or restore with
  `smol machine create --from`.
- **Nothing to set up.** The provider fetches the smolvm engine it was built
  against on first use. It needs KVM on Linux or the Hypervisor framework on
  an Apple silicon Mac, and nothing else: no Docker, no QEMU, no Firecracker.

## Install

Build it from this directory (it is its own Cargo workspace, built on the
[Rust SDK](../../sdk/rust)) and put the binary on your `PATH` or in
`~/.cua/runtime-providers/`:

```bash
cargo build --release
install -m 0755 target/release/cua-runtime-smol ~/.cua/runtime-providers/
cua runtime doctor
```

Cua reads the provider's manifest once per process and notices a provider
installed while `cua daemon` or the Spaces app runs.

## How a Space maps to a machine

| Cua | smol |
| --- | --- |
| Space `dev` | machine `cua-sb-dev`, labelled `cua.runtime=smol`, `cua.name=dev` |
| checkpoint `snap` | frozen machine `cua-ck-snap`, labelled `cua.checkpoint=snap` |
| image | the OCI reference, pulled by the engine (`handles_images: true`) |
| `cpus`, `memory_mb`, `disk_size_gb` | `--cpus`, `--mem`, `--storage` |
| published ports (always including cua-spacesd's 3211) | loopback forwards on free host ports |
| `env` (including `CUA_ENV_TOKEN`) | the workload's environment |
| `restrict_network` | no network device |
| `stop` / `start` | `machine stop` / `machine start --branchable` |
| `suspend` / `resume` | `machine pause` / `machine resume` |
| `exec` | `machine exec` through the guest agent |
| `checkpoint` | `machine branch`, then a throwaway branch with `--freeze-source` |
| `fork` of a Space or a checkpoint | `machine branch` with fresh ports |

The provider keeps one JSON record per Space and per checkpoint under
`~/.smol/cua-runtime/` (`CUA_RUNTIME_SMOL_HOME` moves it): the machine's
name, its image and its port map, because the engine does not report host
ports back to a process that did not create the machine. `cua sb ls` lists
exactly these records.

Machines are started as branch sources so that `fork` and `checkpoint` can
clone them live. `CUA_RUNTIME_SMOL_BRANCHABLE=0` starts them as plain
machines and turns both capabilities off.

## Deleting a Space that has forks

The engine will not delete a machine that is the base of live branches, and
a Space's forks and checkpoints are its branches. When cua deletes such a
Space the provider *retires* its machine instead: it disappears from `cua sb
ls`, is stopped as soon as no running branch depends on it, and is deleted by
the garbage collector, which runs before every `list` and `delete`, once its
branches are gone. `cua-runtime-smol gc` runs the collector by hand.

Cua's `cua sb checkpoints rm` removes its own record and never reaches the
provider, so a checkpoint's frozen machine stays until you run

```bash
echo '{}' | CUA_RUNTIME_PROTOCOL=1 cua-runtime-smol checkpoints
echo '{"name":"snap"}' | CUA_RUNTIME_PROTOCOL=1 cua-runtime-smol delete-checkpoint
```

## Limits

- Linux guests only, on the host's architecture.
- `screenshot` is not provided by the runtime: Cua takes screenshots through
  cua-spacesd in the guest, which its images run.
- Private registry credentials passed per Space are refused; log the host in
  to the registry instead.
- A frozen checkpoint keeps its memory resident as a branch base; the
  portable file is the durable form.

## Protocol

[Write an external runtime provider](https://cua.ai/docs/cua-sdk/guides/external-runtimes)
and [External runtime protocol](https://cua.ai/docs/cua-sdk/guides/external-runtime-protocol).
Try a verb without Cua:

```bash
echo '{}' | CUA_RUNTIME_PROTOCOL=1 cua-runtime-smol info
echo '{}' | CUA_RUNTIME_PROTOCOL=1 cua-runtime-smol doctor
```
