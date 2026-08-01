"""End-to-end RL episode loop on smol (cloud target).

Prepare a gym/environment ONCE as a forkable golden, then:

  * RUN   — fan out N isolated rollouts from it (GRPO-style group sampling) and
            grade each without side effects (`reward_fork`).
  * VERIFY — run the gym as a leased *episode* (assigned exactly once, with a TTL
            and heartbeat), record a score, and read it back — the "did it
            improve?" half.

Every rollout is a copy-on-write clone of the golden, so the per-rollout setup
(deps/repo/dataset) is amortized to ~zero and each rollout starts from the exact
same state.

Run against the cloud with a fork-capable (amd64) account::

    SMOL_CLOUD_TOKEN=... python examples/rl_loop.py

Note: `fork`/`fork_batch`/`reward_fork` are live on cloud today; the leased
`assign`/`complete`/`status` episode API needs the lease endpoints deployed.
"""

from __future__ import annotations

import uuid

from smol import ConnectOptions, Machine, MachineConfig

CLOUD = ConnectOptions(target="cloud")  # uses SMOL_CLOUD_TOKEN


def prepare_golden() -> Machine:
    """The prepared, pinned environment — deps/repo/dataset baked in once. Booting
    it `forkable` makes it a live-RAM fork base so every rollout is a near-instant
    CoW clone that shares its warm state."""
    golden = Machine.create(
        MachineConfig(image="python:3.12-slim", forkable=True),
        CLOUD,
    )
    # Bake whatever setup a rollout would otherwise repeat (install deps, stage a
    # dataset, warm a cache). Done ONCE; every clone inherits it.
    golden.exec(["python", "-c", "print('gym prepared once')"]).assert_success()
    return golden


def run_group(golden: Machine, n: int) -> list[float]:
    """RUN half — GRPO-style group sampling: N independent rollouts from ONE
    prepared state, each graded in a throwaway clone so grading never mutates the
    rollout. Returns the per-rollout rewards."""
    clones = golden.fork_batch(count=n, name_prefix="rollout")
    rewards: list[float] = []
    try:
        for i, clone in enumerate(clones):
            # The rollout: drive the policy over exec calls. (Stand-in here: write
            # an "answer" that depends on the rollout index.)
            clone.exec(
                ["python", "-c", f"open('/tmp/answer', 'w').write(str({i} * 6))"]
            ).assert_success()
            # Reward: grade the END STATE in a throwaway clone — reward judging
            # without side effects. The grader's stdout is the reward signal.
            graded = clone.reward_fork(
                [
                    "python",
                    "-c",
                    "print(1.0 if open('/tmp/answer').read() == '42' else 0.0)",
                ]
            )
            rewards.append(float(graded.stdout.strip() or 0.0))
    finally:
        for clone in clones:
            clone.delete()
    return rewards


def eval_checkpoint(golden: Machine, label: str) -> float:
    """VERIFY half — run the gym as a leased episode: a clean worker assigned
    exactly once (idempotent on the lease id), with a TTL + heartbeat, whose score
    you record and read back. Requires the lease endpoints to be deployed."""
    lease_id = f"eval-{label}-{uuid.uuid4().hex[:8]}"
    with golden.assign(
        lease_id, task={"checkpoint": label}, ttl_secs=600, heartbeat_secs=30
    ) as episode:
        # Run the gym on this checkpoint (the task payload is at /run/smol-task.json
        # inside the clone). Stand-in: compute a pass-rate.
        graded = episode.exec(
            ["python", "-c", "print(0.9 if '__label__'.strip() else 0.0)"]
        )
        score = float(graded.stdout.strip() or 0.0)
        episode.complete(reason="done", score=score, result={"checkpoint": label})
    # Collect the recorded number back from the lease (how a trainer aggregates).
    return float(episode.status().get("score") or 0.0)


def main() -> None:
    golden = prepare_golden()
    try:
        rewards = run_group(golden, n=8)
        mean = sum(rewards) / len(rewards)
        print(f"group rewards: {rewards}  mean={mean:.3f}")

        a = eval_checkpoint(golden, "A")
        b = eval_checkpoint(golden, "B")
        print(f"eval  A={a:.3f}  B={b:.3f}  improved={b > a}")
    finally:
        golden.delete()


if __name__ == "__main__":
    main()
