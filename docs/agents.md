# Agents

`smol agent` runs an agent — Claude Code, or any program — in its own machine as
a session of turns. Each turn runs the agent headless and streams what it does;
the machine keeps its files between turns, and a checkpoint after every turn lets
you rewind the session or fork it.

The agent's own conversation lives inside the machine, so a rewind returns its
memory and its files to the same moment. Nothing about the session depends on
where it runs: the same commands drive the local engine or smol cloud (`--cloud`).

```sh
export ANTHROPIC_API_KEY=...          # read at each turn, never stored
smol agent start fixer                 # machine + Claude Code, network limited to the provider
smol agent send fixer "clone github.com/acme/api into /workspace and make the tests pass"
smol agent send fixer "now add a test for the edge case you found"
smol agent log fixer                   # turns, with their results
smol agent rewind fixer 0              # back to right after the first turn
smol agent fork fixer 1 fixer-alt      # an independent session from turn 1
smol agent pause fixer                 # an idle agent holds no CPU; the next send resumes it
smol agent rm fixer
```

## Harnesses

| Harness | Runs | Needs |
|---|---|---|
| `claude-code` (default) | `claude -p` with streaming JSON; each turn resumes the previous conversation | `ANTHROPIC_API_KEY` |
| `command` | any program, with the prompt appended as its last argument (`--program "sh -c"`, `--image`) | — |

## Network

By default a session can reach only its harness's model provider and package
registry. Add hosts with `--allow-host` (repeatable) or lift the limit with
`--open-network`.

## From code

The sessions are the Rust SDK's `smolmachines::agent` module; `smol agent` is a
thin wrapper around it.

```rust
use smolmachines::agent::{Harness, Session, SessionOptions};

let mut session = Session::start(SessionOptions::new("fixer", Harness::ClaudeCode))?;
let turn = session.send("fix the failing test", &mut |event| println!("{event:?}"))?;
session.rewind(0)?;
```

## Notes

- Session records are JSON files under `~/.smol/agents` (`SMOL_AGENTS_DIR`), and
  local checkpoints sit beside them.
- The API key is passed to each turn's process environment. It is never written to
  the machine's configuration or the session record, but the agent process can
  read it while it runs.
- Rewinding or forking restores a checkpoint into a new machine. Pausing such a
  machine and resuming it needs smolvm with the resume-after-restore fix.
