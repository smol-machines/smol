# RL-rollout toolkit for smolcloud

A small, reusable harness that turns the smol SDK's **fork-from-golden** primitive
into an **RL-style rollout engine**: build a reproducible environment once as a
forkable *golden* machine, then fork many copy-on-write *clones*, run an agent
episode in each, grade the resulting guest state with a binary rubric, tear the
clone down, and aggregate latency / throughput / reward / success metrics.

It's modeled on the shape of agentic RL evals like
[mercor/apex-agents](https://huggingface.co/datasets/mercor/apex-agents): a seeded
workspace, a single instruction, and a set of *binary* rubric criteria graded
against the agent's final filesystem. The bundled world is a freely-licensed proxy
with the same loop, so you can prove the infrastructure scales without an
eval-only dataset.

> **Why fork?** A clone inherits the golden's *warm RAM* (memfd `MAP_PRIVATE`) and
> its raw disks (qcow2 copy-on-write on Linux), so each episode starts from a
> *running, byte-identical* VM in well under a second when uncontended — no cold
> boot, no image pull, no container build per episode. That warm-VM reuse is the
> entire point: it's what makes thousands of short rollouts economical.

---

## Files

| File | Role |
|------|------|
| `harness.py` | The reusable library: `World`, `Rubric`/`Criterion`, `RolloutEngine` (build goldens, fork clones, grade, crash-safe teardown, orphan sweep, scale report). **Start here.** |
| `world.py` | The sample environment ("data-pipeline-bugfix") + its 4 binary rubric criteria. Copy this to define your own world. |
| `agents.py` | `scripted_agent` (deterministic, LLM-free — measures infra throughput) and `llm_agent` (a stub showing exactly where a policy model slots in). |
| `run_scale_test.py` | Generic cloud driver. No placement control — use when every node can fork (or you don't care which one). |
| `prod_fork_scale.py` | Prod driver that **pins goldens to a fork-capable node** by cordoning the others during the golden build (admin / control-plane access required). |
| `requirements.txt` | `smolmachines>=1.3.0` (the cloud transport is pure-Python; no local engine needed). |

---

## Quick start

```bash
pip install -r requirements.txt           # the smol SDK
export SMOL_CLOUD_TOKEN=…                  # your cloud token (see Auth below)

# Smoke test: 8 episodes, 1 golden, 4-wide (stays under a small tenant cap)
python run_scale_test.py --total 8 --concurrency 4 --goldens 1

# Larger: spread 4 goldens across nodes, 800 graded episodes, 64-wide
python run_scale_test.py --total 800 --concurrency 64 --goldens 4

# Clean up a crashed run's leftover machines
python run_scale_test.py --cleanup ./rl-run-<timestamp>
```

Each run writes a `rl-run-<timestamp>/` directory with `report.json` (full
summary + per-rollout records) and `machine-ids.txt` (every machine id created,
for crash recovery).

### Auth

The cloud transport reads the bearer token from `SMOL_CLOUD_TOKEN` (or
`--token`), **never** from a config file. Two kinds of token work:

- **Durable tenant API key** (`smk_…`) — best for unattended/CI runs; doesn't
  expire mid-run.
- **Auth0 access token from `smol`** — short-lived. The `smol` CLI refreshes it
  on any cloud call (e.g. `smol machines`) and stores it under `[cloud] api_key`
  in `~/.config/smolvm/config.toml`. A long scale run can outlive it, so prefer
  the durable key for big runs.

Endpoint defaults to `https://api.smolmachines.com` (override with
`SMOL_CLOUD_URL` / `--base-url`).

---

## How a rollout works

```
build_golden(world)                 # Machine.create(forkable=True) → warm, running VM
   │
   └─ for each episode i, on a thread pool:
        golden.fork("…-ep{i}")      # CoW clone: warm RAM + CoW disks, node-local
          │
          ├─ agent(clone, world, i) # the policy acts: exec commands, write files
          │
          ├─ rubric.grade(clone)    # binary criteria checked against guest state
          │
          └─ clone.delete()         # episode done; clone torn down
```

`RolloutEngine.run_scale(world, agent, total, concurrency, goldens=N)` builds `N`
goldens (the scheduler spreads them across nodes), then round-robins `total`
episodes over them at `concurrency`-wide. The SDK is synchronous; concurrency
comes from a `ThreadPoolExecutor` (the HTTP transport releases the GIL on I/O).

Define your own task by copying `world.py`: pick a base `image`, write your
`Rubric` of binary `Criterion`s, and a `scripted`/`llm` agent in `agents.py`.

---

## What we learned running this at scale on prod smolcloud

These are the non-obvious things that bite you. Read them before scaling up.

### 1. Fork-survivability — a clone does **not** inherit the golden's container workload

This is the single most important gotcha. A forked clone inherits the golden's
warm **RAM** and its raw **CoW disks** — but **not** the golden's *container
workload overlay*. On image-based machines the workload runs inside a container
whose writable layer is an overlay **keyed by machine name**. The clone execs
under its *own* name, so it mounts a *fresh, empty* overlay. Consequences:

- Anything you `pip install` or `write_file` onto the **golden** is **invisible**
  to a **clone's `exec`** (different filesystem).
- On a clone, `write_file`/`read_file` and `exec` touch **different**
  filesystems — keep the agent's writes and the grader's reads on the *same*
  channel.

**Rules that keep a world fork-survivable** (the bundled world follows both):

1. Depend only on the **base image** (`python:3.12-slim` already ships Python) —
   never on a runtime-installed package. The grader uses plain `assert`, not
   pytest.
2. Materialize all per-episode state **inside the clone via `exec`** (which uses
   the clone's own overlay), not via golden `seed_files` / `write_file`.

Done this way, every clone is a fully independent, self-materializing episode —
exactly the per-episode isolation an RL fleet needs. (A future engine change
could stack the golden's overlay read-only under the clone's, which would let
goldens pre-bake heavy state; until then, base-image + exec is the contract.)

### 2. Fork is node-local, and forks from one golden serialize

A clone runs on the **same worker** as its golden (CoW needs the golden's pages
local). So:

- One golden fans out only to **one node's** capacity.
- Forks from a single golden **serialize** — each fork briefly freezes the golden
  to snapshot it. Under concurrency, forks from the same golden queue behind each
  other.

**Lever:** use **N goldens**. The scheduler spreads them across nodes, giving you
N independent fork lanes. More goldens → more parallel forks → higher throughput.

### 3. The per-node throughput ceiling is contention-driven (not a fixed cost)

Fork latency is **sub-second when uncontended** and inflates sharply under
concurrent pressure on one golden/node:

| Concurrency | Goldens | fork_ms p50 | episode_ms p50 | throughput |
|------------:|--------:|------------:|---------------:|-----------:|
| 4           | 2       | **0.64 s**  | 2.5 s          | 0.38 r/s   |
| 48          | 6       | **11.7 s**  | 22.7 s         | 0.64 r/s   |

Same engine, same node. Piling concurrency onto a *single* node buys diminishing
aggregate throughput while tail latency explodes (fork p99 hit 48 s, max 84 s at
concurrency 48). **To go faster, scale horizontally** — more goldens and more
fork-capable nodes — not more concurrency per node. The first wall, though, is
usually your tenant cap (next point), not the engine.

### 4. Your tenant's active-machine cap is the first wall

Forking creates real machines that count against your tenant's **active-machine
quota**. "Active" = **goldens + in-flight clones + any not-yet-swept timeout
orphans**. Keep `goldens + concurrency` comfortably under the cap (free tier is
small — e.g. 10). Exceeding it returns `422 machine count quota exceeded`; the
harness classifies that as a `CAP` outcome (no retry — it needs a freed slot, not
an immediate re-fork) rather than crashing.

### 5. Fork-timeout orphans, and the token sweep that reaps them

A fork that times out **client-side** (SDK timeout) may still succeed
**server-side** — the client never receives the id, so the crash-safe id-registry
can't track or delete it. The engine leaks an orphan machine that silently eats
your cap.

The harness defends with a **run-token name sweep**: every machine it creates
carries a unique per-run token in its name, so `sweep_orphans()` can `GET
/v1/machines` and delete anything matching the token. It runs **periodically
mid-run** (sparing everything still tracked — live goldens + in-flight clones — so
it only deletes true orphans) *and* on exit / SIGINT / SIGTERM. This is what keeps
a long, high-concurrency run from drifting into its cap.

### 6. Fork capability is per-node, in two layers

Not every worker can fork. A node is fork-capable only if **both**:

- the **engine** (`smolvm-bin`) sends `FORK <snapshot_dir>` over the libkrun
  control socket, **and**
- **libkrun** compiles the `FORK` verb arm (`checkpoint_for_fork` /
  `krun_set_snapshot`).

The public create API has **no node-affinity knob**, so to land a golden on a
fork-capable node you cordon the others during the build (`prod_fork_scale.py`
does this: set peer nodes to `draining` in the control DB, build, verify
placement, un-cordon). Clones follow their golden's node automatically, so you
only need to pin the *golden*. Multi-arch images (e.g. `python:3.12-slim` is
amd64+arm64) can be scheduled onto an arm node too — cordon every arch you don't
want.

---

## Results — live on prod smolcloud

A graded scale run against prod `api.smolmachines.com`, all goldens pinned to a
fork-capable bare-metal worker (`c3-standard-192-metal`), `python:3.12-slim`,
1 vCPU / 512 MB, with the deterministic `scripted_agent`:

**300 forked episodes · concurrency 48 · 6 goldens**

| Metric | Value |
|---|---|
| Outcomes | **300 / 300 OK** |
| Success rate | **100%** |
| Mean reward | **1.0** |
| Fully-solved rate | **1.0** (all 4 binary criteria) |
| Throughput | 0.64 rollouts/s |
| fork_ms (p50 / p95 / p99) | 11.7 s / 17.4 s / 48.4 s |
| episode_ms (p50 / p95) | 22.7 s / 33.5 s |
| Golden build | ~9 s each |

Every clone forked, ran its episode, was graded, and was deleted; the run-token
sweep reaped 18 fork-timeout orphans automatically; zero machines leaked.

**Takeaways:** the fork-from-golden primitive is **reliable at scale** (100%
success across 300 concurrent-fork episodes with self-healing cleanup). The
throughput limiter is **per-node fork contention + per-clone container
cold-start**, not raw capacity — the fork-capable worker had hundreds of idle
vCPU throughout. Scaling to *thousands* of concurrent rollouts is therefore a
**horizontal** problem: spread goldens across more fork-capable nodes (and lift
the per-tenant machine cap). The two engine wins that would most raise
single-node throughput are (a) stacking the golden's container overlay under
clones so they skip the per-clone container cold-start, and (b) parallelizing the
per-golden fork snapshot.

---

## Plugging in a real policy model

`scripted_agent` does real tool calls (multiple `exec`s + file materialization),
so latency/throughput here faithfully reflect the rollout substrate without model
cost/variance — use it to answer "does the infra hold up at scale?". To train,
swap in a policy: `agents.llm_agent` is a template showing the loop. Drive all
file writes through `exec` (not `write_file`) so the clone's container sees them
(see gotcha #1).

```python
def llm_agent(clone, world, idx) -> dict:
    token = f"ep{idx}-{uuid.uuid4().hex[:8]}"
    clone.exec(["/bin/sh", "-c", f"echo {token} > /work/clone_id"], opts)
    messages = [{"role": "user", "content": world.instruction}]
    while not done and steps < MAX_STEPS:
        action = model.next_action(messages)            # your policy
        if action.kind == "exec":
            r = clone.exec(action.command, ExecOptions(workdir="/work"))
            messages.append(tool_result(r.stdout, r.stderr, r.exit_code))
    return {"clone_token": token}                       # rubric verifies isolation
```

The harness handles `fork → agent → rubric.grade → delete` and aggregates fork
latency, episode latency, reward, throughput, and success rate across the fleet.
