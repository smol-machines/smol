"""A self-contained sample 'world', modeled on the apex-agents task shape.

apex-agents tasks are: a seeded multi-application workspace (~166 files), a
single-turn instruction, and a set of *binary* rubric criteria graded against
the agent's final state. This world is a lighter, fully self-contained,
freely-licensed proxy with the same loop — so you can prove the infrastructure
scales without needing that eval-only dataset (its license forbids training).

Task: a "data pipeline bugfix". The agent must materialize a small workspace, fix
a buggy aggregation so the total is right, and emit /work/output.json. The rubric
grades the final guest filesystem with four binary checks.

FORK-SURVIVABILITY (important, learned the hard way on prod — see README):
  A forked clone inherits the golden's warm RAM and its raw CoW disks, but the
  container *workload* overlay is keyed by machine name and re-derived fresh for
  the clone at exec time. So anything you `pip install` or `write_file` onto the
  GOLDEN is invisible to a CLONE's `exec`. Two rules keep this world honest on
  the fork path:
    1. Depend only on the BASE IMAGE (python:3.12-slim ships python) — never on a
       runtime-installed package like pytest. The grader uses plain `assert`.
    2. Put task state into the clone via `exec` (which uses the clone's own
       overlay), NOT via `write_file`/golden seed_files (a different channel that
       the clone's container does not see).
  This makes every clone a fully independent, self-materializing episode — which
  is exactly the per-episode isolation an RL rollout fleet needs.

The rubric's four binary checks (all evaluated via `exec`, in the clone overlay):
  1. test_passes     — `pipeline.total()` returns the ground-truth total
  2. output_exists   — /work/output.json was written
  3. total_correct   — the value in output.json matches ground truth
  4. isolation_clean — this clone's unique marker is intact (no cross-clone
                       contamination — the property that makes per-episode
                       forks safe to run thousands-at-a-time)
"""

from __future__ import annotations

from smol import ExecOptions

from harness import Criterion, Rubric, World

WORKDIR = "/work"
EXPECTED_TOTAL = 200  # sum of the amount column in data.csv


def _sh(clone, script: str, timeout: int = 60):
    """Run a /bin/sh script in the clone's own overlay and return the result."""
    return clone.exec(["/bin/sh", "-c", script], ExecOptions(workdir=WORKDIR, timeout=timeout))


# A cwd-independent import check: prepend /work explicitly (python -c does NOT
# put cwd on sys.path) and assert the (possibly just-fixed) pipeline is correct.
_CHECK_TOTAL = (
    "import sys; sys.path.insert(0, '/work'); "
    "from pipeline import total; "
    f"assert total() == {EXPECTED_TOTAL}, total()"
)

_CHECK_OUTPUT = (
    "import json; "
    "d = json.load(open('/work/output.json')); "
    f"assert int(d['total']) == {EXPECTED_TOTAL}, d"
)


def _test_passes(clone, world, idx, agent_return) -> bool:
    # The pipeline computes the correct total (the "bug" is fixed).
    return _sh(clone, f"python -c {_q(_CHECK_TOTAL)}").exit_code == 0


def _output_exists(clone, world, idx, agent_return) -> bool:
    return _sh(clone, "test -f /work/output.json").exit_code == 0


def _total_correct(clone, world, idx, agent_return) -> bool:
    return _sh(clone, f"python -c {_q(_CHECK_OUTPUT)}").exit_code == 0


def _isolation_clean(clone, world, idx, agent_return) -> bool:
    # The agent stamped a unique per-episode marker; confirm it survived. If
    # clones leaked into each other this would read the wrong token.
    want = (agent_return or {}).get("clone_token") if isinstance(agent_return, dict) else None
    if not want:
        return False
    r = _sh(clone, "cat /work/clone_id 2>/dev/null")
    return r.exit_code == 0 and r.stdout.strip() == want


def _q(py: str) -> str:
    """Single-quote a python snippet for embedding in `python -c '...'`."""
    return "'" + py.replace("'", "'\"'\"'") + "'"


TASK_MD = """# Task

`pipeline.total()` is supposed to sum the `amount` column of `data.csv`, but it
drops the last row. Fix `pipeline.py` so the total is correct, then write
`/work/output.json` as `{"total": <the correct total>}`.
"""


def build_world(cpus: int = 1, memory_mb: int = 512) -> World:
    return World(
        name="data-pipeline-bugfix",
        image="python:3.12-slim",  # base image only — nothing installed at runtime
        instruction=TASK_MD,
        cpus=cpus,
        memory_mb=memory_mb,
        # Network ON: a clone gets a FRESH container overlay, so its first exec
        # re-prepares the container from the base image — which goes through an
        # image-pull check that needs network even when the layers are already on
        # the node. (The task itself needs no network.)
        golden_network=True,
        seed_files={},         # NOTE: golden seed_files do NOT reach a clone's exec
        setup_cmds=[],         # ...and neither do golden setup_cmds — see module docstring
        rubric=Rubric(criteria=[
            Criterion("test_passes", "pipeline.total() returns the ground truth", _test_passes),
            Criterion("output_exists", "/work/output.json was written", _output_exists),
            Criterion("total_correct", "output.json total equals ground truth", _total_correct),
            Criterion("isolation_clean", "this clone's marker is intact", _isolation_clean),
        ]),
    )
