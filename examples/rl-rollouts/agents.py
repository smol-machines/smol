"""Agents (policies) that act inside a forked clone.

An agent is `fn(clone, world, idx) -> object`. It does what an RL policy would:
exec commands and read/write files in the VM. Its return value is handed to the
rubric (here we return a per-episode token the rubric uses to verify isolation).

Why everything here goes through `exec` (and not `write_file`): on a forked
clone, `write_file`/`read_file` reach a different filesystem than the container
that `exec` runs in — the container's workload overlay is keyed by machine name
and re-derived fresh for the clone, so it never sees golden-seeded or
write_file'd state. Driving the workspace through `exec` keeps the agent's writes
and the grader's reads on the SAME (per-clone) overlay. See the README's
"Fork-survivability" section and world.py's docstring for the full story.

Two agents are provided:

- `scripted_agent`: a deterministic, LLM-free solver. Use it to measure
  *infrastructure* throughput at scale — it does real tool calls (multiple execs
  + file materialization) so it faithfully exercises the same engine paths a
  model would, without the cost/variance of model inference. This is what you run
  to answer "does the rollout substrate hold up at scale?".

- `llm_agent`: a stub showing exactly where a real policy model slots in.
"""

from __future__ import annotations

import base64
import uuid

from smol import ExecOptions

WORKDIR = "/work"

DATA_CSV = "id,amount\n1,10\n2,25\n3,5\n4,60\n5,100\n"

# The correct pipeline (sums every row). The "buggy" version the task describes
# would do rows[:-1]; a real policy would start from the buggy file and fix it.
# The scripted solver writes the fixed version directly.
FIXED_PIPELINE = (
    "import csv\n"
    "def total(path='/work/data.csv'):\n"
    "    rows = list(csv.DictReader(open(path)))\n"
    "    return sum(int(r['amount']) for r in rows)\n"
)

_COMPUTE = (
    "import sys, json; sys.path.insert(0, '/work'); "
    "from pipeline import total; "
    "json.dump({'total': total()}, open('/work/output.json', 'w'))"
)


def _opts():
    return ExecOptions(workdir=WORKDIR, timeout=120)


def _write_via_exec(clone, path: str, content: str) -> None:
    """Materialize a file inside the clone's own overlay via exec.

    base64 keeps arbitrary content (quotes, newlines) safe through the shell —
    the fork-survivable way to seed files a clone's container will actually see.
    """
    b64 = base64.b64encode(content.encode()).decode()
    clone.exec(["/bin/sh", "-c", f"mkdir -p {WORKDIR} && echo {b64} | base64 -d > {path}"], _opts())


def scripted_agent(clone, world, idx) -> dict:
    """Deterministically solve the data-pipeline-bugfix world.

    Exercises: per-clone marker write, workspace materialization, code fix, and
    code exec — all through `exec`, the same primitives a real agent drives, so
    latency/throughput here reflect the true rollout substrate.
    """
    token = f"ep{idx}-{uuid.uuid4().hex[:8]}"

    # 1. Stamp a unique per-episode marker (the rubric checks it survived).
    _write_via_exec(clone, f"{WORKDIR}/clone_id", token)
    # 2. Materialize the workspace data.
    _write_via_exec(clone, f"{WORKDIR}/data.csv", DATA_CSV)
    # 3. Apply the fix (write the correct pipeline).
    _write_via_exec(clone, f"{WORKDIR}/pipeline.py", FIXED_PIPELINE)
    # 4. Produce the required output artifact.
    clone.exec(["python", "-c", _COMPUTE], _opts())

    return {"clone_token": token}


def llm_agent(clone, world, idx) -> dict:
    """Stub: plug a real policy model's tool-calling loop in here.

    Sketch:
        token = f"ep{idx}-{uuid.uuid4().hex[:8]}"
        clone.exec(["/bin/sh","-c", f"echo {token} > /work/clone_id"], opts)
        messages = [{"role": "user", "content": world.instruction}]
        while not done and steps < MAX_STEPS:
            action = model.next_action(messages)         # your model
            if action.kind == "exec":
                r = clone.exec(action.command, ExecOptions(workdir=WORKDIR))
                messages.append(tool_result(r.stdout, r.stderr, r.exit_code))
            ...
        return {"clone_token": token}

    The harness handles fork → (this) → rubric.grade → delete and aggregates
    fork latency, episode latency, reward, throughput, and success rate. Drive
    file writes through `exec` (not write_file) so the clone's container sees
    them — see the module docstring.
    """
    raise NotImplementedError(
        "llm_agent is a template — wire your policy model's tool loop here.")
