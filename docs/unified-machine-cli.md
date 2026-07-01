# Unified machine CLI — one machine concept, location is an attribute

## Problem

Today the CLI decides *which* machine you mean in three inconsistent ways depending
on the verb:

- **Implicit-local** — `create`, `start`, `stop`, `rm`, `ls`, `fork`, `shell`,
  `cp`, `logs`, `status` operate on local smolvm machines with no flag.
- **A `--cloud` flag** — `exec --cloud` retargets the *same* verb at a cloud
  machine (`src/commands/exec.rs`).
- **A parallel `cloud` sub-tree** — `smol cloud ls / rm / deploy / scale / shell`
  (`src/commands/cloud.rs`) plus a separate cloud-only lister
  (`src/commands/machines.rs`).

So the same conceptual action (`list`, `shell`, `rm`) has two or three spellings
that differ only by *where the machine lives*. A user who thinks "I have a machine
called `codex-box`" has to know, and restate, that it is a cloud machine on every
command.

The SDK already does not have this problem: it exposes **one `Sandbox`** and picks
local-smolvm vs remote-smolfleet behind a **`Transport`** (`sdk/node/transport.ts`).
The CLI split is a historical artifact — local verbs shipped first and cloud was
bolted on — not an essential difference.

## Goal

A machine is **one concept**. Its residency (`local` / `cloud`) is an *attribute*
shown in listings and resolved automatically, not a command path the user must
pre-select. Every verb works on either kind. `--local` / `--cloud` remain as
explicit overrides; `smol cloud …` and `exec --cloud` keep working as thin
back-compat aliases.

## The three essential differences (and how the model absorbs each)

These are real and the design must handle them — but each is an *attribute of a
machine's location*, not a reason for two command trees:

1. **Addressing / name collisions.** Local names are unique per host in the local
   data dir; cloud names are unique *per tenant* and also carry a `mach-<id>`. So
   `codex-box` can exist in **both** places at once. → Resolve a bare reference
   within the active target; when a name is genuinely ambiguous, require a
   `local/` or `cloud/` qualifier (the git-remote / `docker context` pattern).
2. **Auth.** Cloud needs a token (`smol auth login`); local needs nothing. →
   Cloud enumeration is **best-effort**: if `cloud_client()` returns "not logged
   in / not configured", cloud is silently skipped, never a hard failure. A plain
   `smol machine ls` still lists local instantly.
3. **Latency / failure surface.** Local ops are synchronous engine calls; cloud
   ops are HTTPS that can `401`/`429`/time out. → Keep the existing
   sync-resolve-creds-then-`block_on` split (`machines.rs` comment); the resolver
   owns the runtime so individual verbs stay simple.

## Reference grammar

```
<ref>            := [<location> "/"] <name-or-id>
<location>       := "local" | "cloud"
```

- `codex-box`            → resolve in the active target (see below).
- `cloud/codex-box`      → force cloud; error if absent there.
- `local/codex-box`      → force local.
- `mach-b0bc…`           → an id starting with `mach-` is unambiguously cloud.
- Ambiguous bare name (exists local **and** cloud, target = auto) → error listing
  both qualified forms and asking the user to pick.

## Active target resolution (precedence, highest first)

1. Per-command flag: `--local` or `--cloud`.
2. A `location/` prefix on the reference itself.
3. The configured **context** (`smol context use cloud|local|auto`, stored in
   settings). Default `auto`.
4. `auto` behavior: prefer a unique match across both backends; only local if not
   logged in to cloud; ambiguous → error.

## Commands

### `smol machine ls` (unified)

```
LOCATION  NAME              ID              STATE     CPUS  MEMORY    SOURCE
local     default           default         running      2  2048 MiB  alpine:3
cloud     codex-box         mach-b0bc4…     started      2  2048 MiB  codercom/code-server
cloud     (unnamed)         mach-5bb17…     stopped      1  1024 MiB  -
```

- Lists **both** backends. Cloud is best-effort: if not logged in, a dim
  `# cloud: not logged in (run 'smol auth login')` line is printed to stderr and
  local still lists.
- `--local` / `--cloud` restrict to one. `--json` emits a flat array with a
  `location` field on every row.

### `smol context`

```
smol context show           # prints active target (auto|local|cloud)
smol context use cloud      # persist the default target
```

Thin wrapper over settings; no network. `auto` is the default so nothing breaks
for existing users.

### Every other verb

`exec`, `shell`, `start`, `stop`, `rm`, `logs`, `status`, `cp`, `fork` all take a
`<ref>` and route through the resolver. `exec --cloud` becomes sugar for
`exec cloud/<name>`; `smol cloud <sub>` becomes sugar that sets target = cloud and
dispatches the shared verb.

## Back-compat & migration

- `--cloud` on `exec` and the `smol cloud …` sub-tree stay, unchanged, as aliases
  that set the target — no breakage, no deprecation warning in the first release.
- `smol machines` (cloud-only lister) becomes `smol ls --cloud`.
- New surface (`context`, unified `ls`, `location/` prefixes) is additive.
- A later release can emit a soft deprecation note on the aliases once the unified
  surface is documented as canonical.

## Why this is a CLI-only change

No engine and no control-plane change: both backends are **already** unified one
layer down (SDK `Transport`; the CLI's own `cloud::list_machines` +
`SmolvmConfig::list_vms`). This is a resolver + a listing merge + a context knob on
top of helpers that already exist.

## Prototype status

Landed in this branch:

- `src/commands/resolve.rs` — `Location`, `MachineRef`, `Target`, `list_all()`
  (enumerates both, cloud best-effort), the ambiguity-strict `resolve()`, and the
  two routing policies verbs actually use: `locate()` (bare-name miss is an
  error — for `exec`/`shell`, which need an existing machine) and `route()`
  (bare-name miss falls back to local — for the lifecycle verbs, which own their
  local not-found / create-first / bare-`default` messaging).
- `src/commands/ls.rs` — the unified lister with a `LOCATION` column, driven by
  `resolve::list_all`.
- **Every location-bearing verb routes by location, not a command path.** Each
  gained a `--local` flag (mutually exclusive with `--cloud`) and dispatches on
  the resolver's decision:
  - `exec` / `shell` → `locate()` (`shell` is an `ExecCmd` built in `main.rs`).
  - `start`, `stop`, `logs`, `status` → `route()` on `--name`.
  - `fork` → `route()` on `--golden` (the clone lands wherever its golden lives).
  - `cp` → `route()` on the `machine:path` ref.
  - `rm` → `route()` on `--name`, and **gained cloud deletion** (previously only
    reachable via `smol cloud rm`); it confirms once regardless of location.

  `--cloud` and the `cloud/`/`local/` prefixes are now sugar over the resolver;
  `smol cloud …` remains as an explicit alias sub-tree.

### Noun-first grouping (`smol machine <verb>`)

The lifecycle verbs no longer sit at the top level. Every machine operation now
hangs off the `machine` noun, Docker/podman "management command" style:

```
smol machine create | start | stop | rm | ls | status
             | exec | shell | logs | cp | fork
             | images | prune | update | monitor | data-dir
```

- This was a deliberate **breaking** move (chosen over keeping top-level
  shortcuts): `smol create`, `smol ls`, `smol start`, … now error with
  `unrecognized subcommand`. There is one spelling for each action, so the
  surface reads as "a machine, and the things you do to it."
- Familiar names survive as `visible_alias`es: `smol machine rm`↔`delete`,
  `smol machine ls`↔`list`, `smol machine shell`↔`sh` (shown in `--help`).
- The top level keeps only non-machine flows: `run` (ephemeral), `init`/`up`/
  `down` (Smolfile), `pack`, `auth`, `cloud`, `config`.
- **Nothing about routing changed** — each verb still resolves location via
  `resolve::route()`/`locate()`. `smol machine status -n codex-box` auto-routes
  to a cloud machine with no flag exactly as the top-level verb did.

Not yet wired (design, next steps): the persistent `smol context use
cloud|local|auto` settings field, and folding the `smol cloud …` sub-tree into
thin alias shims over the unified verbs.

Docs pass done alongside the move: this repo's `README.md` and `docs/cli.md`, and
every user-facing error/help string in `src/commands/*.rs`, now spell the moved
verbs `smol machine <verb>`. A sweep of the SDK examples (`examples/`, the Node /
Python SDK dirs) and the public docs site (`smolmachines/`) found no old
top-level spellings — the SDKs never shelled out to the CLI, so they were already
clean.
