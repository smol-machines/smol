# smol CLI reference

`smol` manages microVM sandboxes locally (in-process, via the bundled smolvm
engine) and on the **smolfleet** cloud. Run `smol <command> --help` for the
exact flags of any command — this page is the map.

Global behavior:
- Config lives at `~/.config/smolvm/config.toml` (override with `SMOLVM_CONFIG`);
  the file is created `0600`. Secrets (cloud API keys) are masked by `config show`.
- Logging is controlled by `RUST_LOG` (default `warn`); set `RUST_LOG=debug` for
  verbose output.
- Commands are grouped under nouns: `smol machine …` (a machine's whole
  lifecycle), `smol file …` (Smolfile), `smol pack …` (artifacts),
  `smol registry …` (registries), `smol auth …` (login), `smol cloud …`
  (smolfleet). `smol run` (ephemeral one-shot) stays top-level.
- **Local or cloud is resolved automatically.** `smol machine …` commands find a
  machine wherever it lives — local engine or smolfleet — so `smol machine ls`
  shows both. Force one side with `--local` / `--cloud`, or with a `local/<name>`
  / `cloud/<name>` prefix (a `mach-…` id is always cloud).

## Ephemeral one-shot — `smol run`

| Command | Description |
|---------|-------------|
| `smol run <image> -- <cmd…>` | Boot an **ephemeral** machine, run a command, stream output, exit with its code, then discard the machine. |

## Machine lifecycle — `smol machine …`

| Command | Description |
|---------|-------------|
| `smol machine create <name> --image <ref>` | Create a **persistent** machine (does not run a command). Use `--from <path.smolmachine>` to create from a packed artifact (uses its layers, no pull). Restrict egress with `--allow-cidr`/`--allow-host`/`--outbound-localhost-only` (imply `--net`). |
| `smol machine start <name>` / `smol machine stop <name>` | Start / stop a persistent machine. Add `--forkable` to `start` to make it a fork base. |
| `smol machine ls` | List machines, local **and** cloud (`--json`; `--local`/`--cloud` to scope). Alias: `list`. |
| `smol machine status <name>` | Show one machine's status (`--json`). |
| `smol machine rm <name>` | Delete a machine (`--force` to skip the confirmation prompt). Alias: `delete`. |
| `smol machine exec --name <name> -- <cmd…>` | Run a command in a machine (`--stream` for live output; `-e KEY=VALUE`, `-w DIR`, `-i/-t`). Inject secrets host-side for the call with `--secret-env GUEST=HOST_VAR` / `--secret-file GUEST=/path` (never persisted). |
| `smol machine shell --name <name>` | Interactive shell into a machine. Alias: `sh`. |
| `smol machine cp <src> <dst>` | Copy files host↔guest (`name:/path` denotes a guest path). |
| `smol machine logs <name>` | Fetch machine logs (`--tail N`). |
| `smol machine fork --golden <name> --name <clone>` | Clone a running, forkable machine (copy-on-write RAM + disks). The golden must have been started `--forkable`; it then stays frozen as the shared base. `-p HOST:GUEST` pins the clone's ports (otherwise they're remapped to free host ports). |
| `smol machine images --name <name>` | List a machine's cached images and storage usage (`--json`). |
| `smol machine prune --name <name>` | Reclaim a machine's disk: free unreferenced layers, or `--all` to purge the cache (`--dry-run` to preview; `--all` requires the machine stopped). |
| `smol machine update --name <name> …` | Modify a **stopped** machine: add/remove volumes/ports/env, set `--cpus`/`--mem`/`--workdir`, toggle `--net`/`--gpu`, expand `--storage`/`--overlay` (expand-only). |
| `smol machine monitor --name <name>` | Supervise a machine in the foreground with health checks (`--health-cmd`, `--interval`, `--health-retries`) and a restart policy (`--restart never\|always\|on-failure\|unless-stopped`). |
| `smol machine data-dir --name <name>` | Print the machine's on-disk data directory (scripting/debugging). |

## Smolfile (declarative) — `smol file …`

| Command | Description |
|---------|-------------|
| `smol file init` | Scaffold a `Smolfile` in the current directory. |
| `smol file up` / `smol file down` | Bring the `Smolfile`-defined machine up / down. |

## Artifacts — `smol pack …`

| Command | Description |
|---------|-------------|
| `smol pack create …` | Build a packable `.smolmachine` image artifact. |
| `smol pack push <ref>` / `smol pack pull <ref>` | Push / pull artifacts to/from a registry. |
| `smol pack inspect <ref>` | Inspect an artifact in a registry. |

## Registries — `smol registry …`

| Command | Description |
|---------|-------------|
| `smol registry ls` | List the registries you have stored credentials for. |
| `smol registry catalog [host]` | List repositories in a registry (`GET /v2/_catalog`; `--json`). Not every registry exposes the catalog endpoint. |
| `smol registry tags <reference>` | List the tags of a repository (`--json`). |
| `smol registry login` / `smol registry logout` | Authenticate to / forget a registry (aliases of `smol auth login` / `logout`). |

## Authentication — `smol auth …`

| Command | Description |
|---------|-------------|
| `smol auth login` / `smol auth logout` | Authenticate to the registry and cloud (OAuth device flow) — one token covers both the artifact registry and the smolfleet API. |

## Cloud (smolfleet) — `smol cloud …`

Cloud machines are also reachable through `smol machine …` with `--cloud` (or a
`cloud/<name>` prefix); this group holds the cloud-only operations.

| Command | Description |
|---------|-------------|
| `smol cloud deploy --image <ref>` | Create + start a machine on the cloud cluster. Scope egress with `--allow-host`/`--allow-cidr`. |
| `smol cloud ls` | List cloud machines. |
| `smol cloud rm <id>` | Delete a cloud machine. |
| `smol cloud scale …` | Scale a cloud deployment. |
| `smol cloud shell --name <name>` | Interactive shell into a cloud machine. Alias: `sh`. |

> **Scoped egress and image pulls.** A cloud machine pulls its base image the
> first time it runs a command, so a `--allow-host` scope automatically also
> permits the image's own registry (e.g. `docker.io` for Docker Hub) — you do
> not need to list it yourself. A registry that serves image blobs from an
> unrelated CDN (some private registries) may still need an extra `--allow-host`
> for that CDN; the pull error names the host it could not reach.

## Config — `smol config …`

| Command | Description |
|---------|-------------|
| `smol config set <key> <value>` | Set a config value (e.g. cloud endpoint, API key). Plain-HTTP endpoints are rejected for non-loopback hosts. |
| `smol config show` | Show config (secrets masked). |

> This reference is a map of the command surface; for authoritative flags and
> defaults always use `smol <command> --help`.
