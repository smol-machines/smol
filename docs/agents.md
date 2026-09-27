# Agents

`smol agent` runs an agent — Claude Code, Codex, OpenCode, or any program — in its
own machine as a session of turns. Each turn runs the agent headless and streams what it does;
the machine keeps its files between turns, and a checkpoint after every turn lets
you rewind the session or branch it.

The agent's own conversation lives inside the machine, so a rewind returns its
memory and its files to the same moment. Without `--cloud`, the CLI owns the
session record on this computer. With `--cloud`, the hosted API owns the session
and continues its turns after the CLI disconnects; every command addresses the
hosted session explicitly.

## Local sessions

```sh
export ANTHROPIC_API_KEY=...          # stays on this host; the machine sees a placeholder
smol agent start fixer                 # machine + Claude Code, network limited to the provider
smol agent send fixer "clone github.com/acme/api into /workspace and make the tests pass"
smol agent send fixer "now add a test for the edge case you found"
smol agent log fixer                   # turns, with their results
smol agent rewind fixer 0              # back to right after the first turn
smol agent branch fixer 1 fixer-alt    # an independent session from turn 1
smol agent pause fixer                 # an idle agent holds no CPU; the next send resumes it
smol agent rm fixer
```

The former `smol agent fork` command remains an alias for `branch`.

## Hosted sessions

Log in with `smol auth login` or set `SMOL_CLOUD_TOKEN`. Store a model key as a
smol cloud credential once, then name it with `--credential` when starting the
session. The key stays outside the agent's machine.

```sh
smol agent start --cloud fixer --credential anthropic
smol agent send --cloud fixer "make the tests pass"    # follows the turn's events
smol agent log --cloud fixer                           # status and turn history
smol agent branch --cloud fixer 0 fixer-alt
smol agent pause --cloud fixer
smol agent rm --cloud fixer
```

`start --cloud` waits for harness setup, and `send --cloud` waits if setup is
still running. The session itself lives in smol cloud, so closing the CLI does
not stop setup or an accepted turn. `send --cloud` reconnects to the event
stream after a transient disconnect. Use the same `--cloud` flag with `ls`,
`rewind`, `resume`, `cancel`, and `rm`; local and hosted sessions may share a name.

If you have not stored a credential, pass the model key from the current shell
for each turn without putting its value on the command line:

```sh
export ANTHROPIC_API_KEY=...
smol agent start --cloud fixer
smol agent send --cloud fixer --env-from ANTHROPIC_API_KEY "make the tests pass"
```

`--idempotency-key` makes a retried `send --cloud` return the same turn, and
`--timeout-seconds` sets that turn's maximum run time. `--no-template` and
`--pause-between-turns` apply to local sessions; hosted sessions offer explicit
`pause` and `resume`.

## Harnesses

| Harness | Runs | Needs |
|---|---|---|
| `claude-code` (default) | `claude -p` with streaming JSON; each turn resumes the previous conversation | `ANTHROPIC_API_KEY` |
| `codex` | `codex exec --json`; each turn resumes the previous thread (`--model` optional) | `OPENAI_API_KEY` |
| `opencode` | `opencode run --format json` with `--model provider/model`; each turn continues the previous session | the provider's key: `ANTHROPIC_API_KEY` for `anthropic/*`, `OPENAI_API_KEY` for `openai/*`, none for OpenCode's own `opencode/*` models |
| `command` | any program, with the prompt appended as its last argument (`--program "sh -c"`, `--image`) | — |

## Templates

The first local session of a given setup installs its harness, then saves a template:
a checkpoint of the machine taken right after the install. Later sessions with
the same setup — harness, model, network hosts, key handling, size — start from
it in a few seconds instead of installing again. Templates live in
`~/.smol/agents/templates`, hold a whole machine (several hundred MB each), and
are rebuilt after a week so the harness stays current. `--no-template` installs
from scratch. Templates are local; cloud sessions always install.

## Network

By default a session can reach only its harness's model provider and package
registry. Add hosts with `--allow-host` (repeatable) or lift the limit with
`--open-network`.

## As a service

`smol agent serve` exposes self-hosted sessions over HTTP. Turns run inside the
service, not in the client: a client starts a turn, gets its number, and can
disconnect — the turn keeps going. Its events are kept, so any client can stream
them from the start or from where it left off.

```sh
SMOL_AGENTS_TOKEN=... smol agent serve --listen 127.0.0.1:7777

curl -X POST localhost:7777/v1/agents -H "Authorization: Bearer $T" \
  -d '{"name":"fixer","harness":"claude-code"}'   # or "codex", or "opencode" with "model"
curl -X POST localhost:7777/v1/agents/fixer/turns -H "Authorization: Bearer $T" \
  -d '{"prompt":"make the tests pass","env":{"ANTHROPIC_API_KEY":"..."}}'   # -> {"turn":0}
curl -N localhost:7777/v1/agents/fixer/turns/0/events -H "Authorization: Bearer $T"   # SSE
curl -N "localhost:7777/v1/agents/fixer/turns/0/events?after=41" ...          # resume after event 41
```

| Route | |
|---|---|
| `POST /v1/agents` | start a session (`name`, `harness`, `image`, `program`, `cloud`, `allowHosts`, `openNetwork`, `checkpoints`, `pauseBetweenTurns`, `cpus`, `memoryMib`) |
| `GET /v1/agents`, `GET /v1/agents/{name}` | sessions; the latter includes `runningTurn` |
| `POST /v1/agents/{name}/turns` | start a turn (`prompt`, `env`) → `202 {"turn": n}`; `409` while one runs |
| `GET /v1/agents/{name}/turns/{n}/events?after=K` | the turn's events as SSE (`event` per agent event, `id` = its number, then `done`) |
| `POST /v1/agents/{name}/rewind` / `branch` / `pause` / `resume`, `DELETE /v1/agents/{name}` | as the CLI |

The service refuses to listen beyond loopback without a token. Two things to know
before sharing one:

- It runs sessions with **its own** smol cloud credentials (for `"cloud": true`),
  so everyone holding the token uses that account. Run one per user or team.
- Local sessions use the model key from the service's own environment. Cloud
  sessions take it in a turn's `env`, which travels in the request body; keep
  the service on loopback or behind TLS.

## From code

Local sessions use the Rust SDK's `smolmachines::agent` module; hosted sessions
use `smolmachines::cloud_agent::CloudAgentSession`.

```rust
use smolmachines::agent::{Harness, Session, SessionOptions};

let mut session = Session::start(SessionOptions::new("fixer", Harness::ClaudeCode))?;
let turn = session.send("fix the failing test", &mut |event| println!("{event:?}"))?;
session.rewind(0)?;
```

## Notes

- Local session records are JSON files under `~/.smol/agents` (`SMOL_AGENTS_DIR`),
  and local checkpoints sit beside them. Hosted sessions are listed by
  `smol agent ls --cloud` and have no local record.
- On the local engine, when `ANTHROPIC_API_KEY` is set as the session starts, the
  key never enters the machine: the agent sees a placeholder, and the engine
  swaps in the real key only on HTTPS requests to `api.anthropic.com`. The engine
  reads the key from the environment of whichever command starts or resumes the
  machine, so keep it set there. A turn that passes the key in its own `env` is
  refused, since that would put it inside the machine.
- Hosted sessions can bind a stored cloud credential, keeping the key outside
  the machine. With `--env-from` instead, the key is passed in that turn's
  process environment; the agent can read it while it runs.
- Rewinding or branching restores a checkpoint into a new machine. Pausing such a
  machine and resuming it needs smolvm with the resume-after-restore fix.
