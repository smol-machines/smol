#!/usr/bin/env python3
"""Drive a graded RL-rollout scale test against smolcloud and emit a report.

Examples:
    # Smoke test (a handful of rollouts, one golden — stays under the default
    # per-tenant concurrency cap):
    python run_scale_test.py --total 8 --concurrency 4 --goldens 1

    # Fleet-scale: spread 4 goldens across nodes, 800 graded episodes @ 64-wide.
    python run_scale_test.py --total 800 --concurrency 64 --goldens 4

    # Clean up any machines left by a crashed run (deletes ids in a results dir):
    python run_scale_test.py --cleanup ./rl-run-1719000000

Auth: set SMOL_CLOUD_TOKEN (smk_…) or pass --token. Endpoint defaults to
https://api.smolmachines.com (override with SMOL_CLOUD_URL or --base-url).
"""

from __future__ import annotations

import argparse
import json
import os
import sys

from smol import ConnectOptions, Machine

from agents import scripted_agent
from harness import RolloutEngine
from world import build_world


def _conn(args) -> ConnectOptions:
    return ConnectOptions(target="cloud", api_key=args.token, base_url=args.base_url)


def _write_markdown(report, path: str) -> None:
    s = report.summary()
    fork = s["fork_ms"]
    lines = [
        f"# smolcloud RL-rollout scale report — `{s['world']}`",
        "",
        f"- **Rollouts:** {s['total_rollouts']} @ concurrency {s['concurrency']} over {s['goldens']} golden(s)",
        f"- **Wall time:** {s['wall_seconds']}s → **{s['throughput_rollouts_per_sec']} rollouts/sec**",
        f"- **Success rate:** {s['success_rate']}  (fully-solved {s['fully_solved_rate']}, mean reward {s['mean_reward']})",
        f"- **Outcomes:** {s['outcomes']}",
        f"- **Fork latency (ms):** p50 {fork['p50']} · p95 {fork['p95']} · p99 {fork['p99']} · max {fork['max']}",
        f"- **Episode latency (ms):** p50 {s['episode_ms']['p50']} · p95 {s['episode_ms']['p95']}",
        f"- **Golden build (s):** {s['golden_build_seconds']}",
        "",
        "Outcomes legend: `OK` graded episode · `CAP` per-tenant concurrency cap "
        "· `FAIL` fork/start rejected · `ERROR` harness/exec error.",
    ]
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def cmd_cleanup(args) -> int:
    ids_path = os.path.join(args.cleanup, "machine-ids.txt")
    if not os.path.exists(ids_path):
        print(f"no machine-ids.txt under {args.cleanup}")
        return 1
    conn = _conn(args)
    ids = [x.strip() for x in open(ids_path) if x.strip()]
    print(f"deleting {len(ids)} tracked machines…")
    for mid in ids:
        try:
            Machine.connect(mid, conn).delete()
            print(f"  deleted {mid}")
        except Exception as e:
            print(f"  skip {mid}: {e}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--total", type=int, default=8, help="total graded rollouts")
    ap.add_argument("--concurrency", type=int, default=4, help="max in-flight rollouts")
    ap.add_argument("--goldens", type=int, default=1,
                    help="number of golden bases (spread across nodes for fleet scale)")
    ap.add_argument("--cpus", type=int, default=1)
    ap.add_argument("--memory-mb", type=int, default=512)
    ap.add_argument("--token", default=os.environ.get("SMOL_CLOUD_TOKEN"),
                    help="cloud API key (smk_…); defaults to $SMOL_CLOUD_TOKEN")
    ap.add_argument("--base-url", default=os.environ.get("SMOL_CLOUD_URL"),
                    help="control-plane URL; defaults to $SMOL_CLOUD_URL or prod")
    ap.add_argument("--results-dir", default=None)
    ap.add_argument("--cleanup", default=None,
                    help="delete machines tracked in the given results dir, then exit")
    args = ap.parse_args()

    if args.cleanup:
        return cmd_cleanup(args)

    if not args.token:
        print("ERROR: no cloud token. Set SMOL_CLOUD_TOKEN or pass --token.", file=sys.stderr)
        return 2

    world = build_world(cpus=args.cpus, memory_mb=args.memory_mb)
    eng = RolloutEngine(_conn(args), results_dir=args.results_dir)
    print(f"[run] results dir: {eng.results_dir}")

    report = eng.run_scale(world, scripted_agent, total=args.total,
                           concurrency=args.concurrency, goldens=args.goldens)
    report.print_summary()

    md = os.path.join(eng.results_dir, "report.md")
    _write_markdown(report, md)
    print(f"\n[run] wrote {os.path.join(eng.results_dir, 'report.json')} and {md}")

    # Non-zero exit if reliability looks bad, so CI can gate on it.
    s = report.summary()
    if s["success_rate"] is not None and s["success_rate"] < 0.95:
        print(f"[run] success_rate {s['success_rate']} < 0.95", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
