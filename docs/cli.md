# smol CLI reference

`smol` manages microVM sandboxes locally (in-process, via the bundled smolvm
engine) and on the **smolfleet** cloud. Run `smol <command> --help` for the
exact flags of any command — this page is the map.

Global behavior:
- Config lives at `~/.config/smolvm/config.toml` (override with `SMOLVM_CONFIG`);
  the file is created `0600`. Secrets (cloud API keys) are masked by `config show`.
- Logging is controlled by `RUST_LOG` (default `warn`); set `RUST_LOG=debug` for
  verbose output.
- `--cloud` on lifecycle commands targets the configured smolfleet cluster
  instead of the local engine.

## Local machine lifecycle

| Command | Description |
|---------|-------------|
| `smol run <image> -- <cmd…>` | Boot an **ephemeral** machine, run a command, stream output, exit with its code. |
| `smol create <name> --image <ref>` | Create a **persistent** machine (does not run a command). Use `--from <path.smolmachine>` to create from a packed artifact (uses its layers, no pull). Restrict egress with `--allow-cidr`/`--allow-host`/`--outbound-localhost-only` (imply `--net`). |
| `smol start <name>` / `smol stop <name>` | Start / stop a persistent machine. Add `--forkable` to `start` to make it a fork base. |
| `smol fork --golden <name> --name <clone>` | Clone a running, forkable machine (copy-on-write RAM + disks). The golden must have been started `--forkable`; it then stays frozen as the shared base. `-p HOST:GUEST` pins the clone's ports (otherwise they're remapped to free host ports). |
| `smol ls` | List machines (`--json` for machine-readable output). |
| `smol status <name>` | Show one machine's status (`--json`). |
| `smol images --name <name>` | List a machine's cached images and storage usage (`--json`). |
| `smol prune --name <name>` | Reclaim a machine's disk: free unreferenced layers, or `--all` to purge the cache (`--dry-run` to preview; `--all` requires the machine stopped). |
| `smol update --name <name> …` | Modify a **stopped** machine: add/remove volumes/ports/env, set `--cpus`/`--mem`/`--workdir`, toggle `--net`/`--gpu`, expand `--storage`/`--overlay` (expand-only). |
| `smol monitor --name <name>` | Supervise a machine in the foreground with health checks (`--health-cmd`, `--interval`, `--health-retries`) and a restart policy (`--restart never\|always\|on-failure\|unless-stopped`). |
| `smol rm <name>` | Delete a machine (`--force` to skip the confirmation prompt). |

## Exec, shell & files

| Command | Description |
|---------|-------------|
| `smol exec --name <name> -- <cmd…>` | Run a command in a machine (`--stream` for live output; `-e KEY=VALUE`, `-w DIR`, `-i/-t`). Inject secrets host-side for the call with `--secret-env GUEST=HOST_VAR` / `--secret-file GUEST=/path` (never persisted). |
| `smol shell --name <name>` | Interactive shell into a machine. |
| `smol cp <src> <dst>` | Copy files host↔guest (`name:/path` denotes a guest path). |
| `smol logs <name>` | Fetch machine logs (`--tail N`). |
| `smol data-dir --name <name>` | Print the machine's on-disk data directory (scripting/debugging). |

## Smolfile (declarative)

| Command | Description |
|---------|-------------|
| `smol init` | Scaffold a `Smolfile` in the current directory. |
| `smol up` / `smol down` | Bring the `Smolfile`-defined machine up / down. |

## Container registry

| Command | Description |
|---------|-------------|
| `smol pack create …` | Build a packable image artifact. |
| `smol push <ref>` / `smol pull <ref>` | Push / pull images to/from a registry. |
| `smol inspect <ref>` | Inspect an image. |
| `smol login` / `smol logout` | Authenticate to the registry / cloud (OAuth device flow). |

## Cloud (smolfleet)

| Command | Description |
|---------|-------------|
| `smol deploy --image <ref>` | Create + start a machine on the cloud cluster. |
| `smol machines` | List cloud machines. |
| `smol destroy <id>` | Delete a cloud machine. |
| `smol scale …` | Scale a cloud deployment. |
| `smol exec\|start\|stop\|status\|logs --cloud …` | Operate on cloud machines. |

## Config

| Command | Description |
|---------|-------------|
| `smol config set <key> <value>` | Set a config value (e.g. cloud endpoint, API key). Plain-HTTP endpoints are rejected for non-loopback hosts. |
| `smol config show` | Show config (secrets masked). |

> This reference is generated from the command surface; for authoritative flags
> and defaults always use `smol <command> --help`.
