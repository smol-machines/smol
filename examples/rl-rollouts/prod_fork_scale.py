#!/usr/bin/env python3
"""Prod fork-scale driver: pin a golden to a fork-capable worker, then fan out.

This is the *prod-specific* wrapper around the reusable `harness` library. It
exists because of one operational fact about the current smolcloud fleet: only
some workers carry a fork-capable engine build, and the public create API has no
node-affinity knob. Fork is node-local (a clone is pinned to its golden's node),
so we only need to land the *golden* on the fork-capable worker — every clone
follows automatically.

How it pins placement (admin, control-plane side):
  1. Cordon every *other* schedulable x86 node (set status='draining' in the
     control DB). Cordon ≠ drain: running VMs are untouched, only new placement
     stops. The arm worker is already excluded by the image's arch.
  2. Build the forkable golden(s) via the tenant API — with the other nodes
     cordoned they land deterministically on the fork-capable worker.
  3. Verify each golden's node_id in the control DB (fail closed if any is wrong).
  4. Un-cordon immediately — the golden build is the only cordon window; the
     fan-out runs with the fleet fully schedulable.
  5. Fork `total` clones at `concurrency` (round-robined over the goldens), grade
     + delete each (the harness's crash-safe registry tears everything down on
     exit). More goldens => higher fork throughput (forks serialize per golden).

Everything runs in ONE process so the harness's atexit/SIGINT teardown still
guarantees no leaked machines, even on crash.

Usage:
  PYTHONPATH=.:../../sdk/python/python \
  SMOL_CLOUD_TOKEN=$(…) python3 prod_fork_scale.py --total 16 --concurrency 8

Admin levers (cordon + placement-verify) shell out to `gcloud ssh` on the
control box and run psql against the control DB. They are no-ops if
--no-pin is passed (e.g. when every node is fork-capable).
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys

from smol import ConnectOptions

from agents import scripted_agent
from harness import RolloutEngine, machine_ref
from world import build_world

# --- prod fleet facts (control-plane side) ---------------------------------
CONTROL = "smolcloud-control"
ZONE = "us-central1-a"
PROJECT = "smolmachines"
# The fork-capable worker the golden must land on.
FORK_NODE_ID = "smolcloud-x86-metal-worker-1-724edd70a3df"
# Every OTHER schedulable node to cordon during the golden build so the golden
# can only land on FORK_NODE_ID. The arm node must be included too: the test
# image is a multi-arch manifest, so the scheduler can legitimately place it on
# arm — cordoning only the x86 peer is not enough.
CORDON_NODE_IDS = [
    "smolcloud-x86-metal-worker-0-c26984193f5b",
    "smolcloud-arm-worker-0us-central-50b2531a33b2",
]


def _control_psql(sql: str) -> str:
    """Run one SQL statement on the control DB via gcloud ssh; return stdout."""
    remote = (
        'PGURL=$(sudo cat /etc/smolfleet/postgres-url); '
        f'psql "$PGURL" -tA -c {_shq(sql)}'
    )
    out = subprocess.run(
        ["gcloud", "compute", "ssh", CONTROL, "--zone", ZONE, "--project", PROJECT,
         "--tunnel-through-iap", "--command", remote],
        capture_output=True, text=True, timeout=120,
    )
    if out.returncode != 0:
        raise RuntimeError(f"control psql failed: {out.stderr.strip()[:300]}")
    # Strip the gcloud SSH/NumPy warning lines; keep psql output.
    lines = [l for l in out.stdout.splitlines()
             if l.strip() and "WARNING" not in l and "NumPy" not in l
             and "performance" not in l and "please see" not in l]
    return "\n".join(lines).strip()


def _shq(s: str) -> str:
    """Single-quote a string for safe embedding inside the remote bash -c."""
    return "'" + s.replace("'", "'\"'\"'") + "'"


def set_node_status(node_id: str, status: str) -> None:
    _control_psql(f"UPDATE nodes SET status='{status}' WHERE id='{node_id}';")


def golden_node_id(mach_id: str) -> str:
    return _control_psql(
        f"SELECT node_id FROM machines WHERE id='{mach_id}';").strip()


def cordon_others() -> None:
    for nid in CORDON_NODE_IDS:
        set_node_status(nid, "draining")
    print(f"[pin] cordoned {len(CORDON_NODE_IDS)} node(s): {CORDON_NODE_IDS}")


def uncordon_others() -> None:
    for nid in CORDON_NODE_IDS:
        try:
            set_node_status(nid, "ready")
        except Exception as e:
            print(f"[pin] WARNING: failed to un-cordon {nid}: {e}", file=sys.stderr)
    print(f"[pin] un-cordoned {len(CORDON_NODE_IDS)} node(s)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--total", type=int, default=16)
    ap.add_argument("--concurrency", type=int, default=8)
    ap.add_argument("--goldens", type=int, default=1,
                    help="number of goldens (all pinned to the fork-capable node); "
                         "more goldens => higher fork throughput (forks serialize per golden)")
    ap.add_argument("--cpus", type=int, default=1)
    ap.add_argument("--memory-mb", type=int, default=512)
    ap.add_argument("--token", default=os.environ.get("SMOL_CLOUD_TOKEN"))
    ap.add_argument("--base-url", default=os.environ.get("SMOL_CLOUD_URL"))
    ap.add_argument("--results-dir", default=None)
    ap.add_argument("--no-pin", action="store_true",
                    help="skip cordon/placement-pin (use when all nodes can fork)")
    args = ap.parse_args()

    if not args.token:
        print("ERROR: set SMOL_CLOUD_TOKEN or pass --token", file=sys.stderr)
        return 2

    conn = ConnectOptions(target="cloud", api_key=args.token, base_url=args.base_url)
    world = build_world(cpus=args.cpus, memory_mb=args.memory_mb)
    eng = RolloutEngine(conn, results_dir=args.results_dir)
    print(f"[run] results dir: {eng.results_dir}")

    pinned = not args.no_pin

    def before_build():
        # Cordon every other node so all goldens land on the fork-capable worker.
        if pinned:
            cordon_others()

    def after_build(goldens):
        # Verify placement, then ALWAYS un-cordon (the cordon window is the build
        # only — never the fan-out). If any golden landed off-target, raise so
        # run_scale tears the goldens down and aborts before forking.
        try:
            if pinned:
                wrong = []
                for g in goldens:
                    gid = machine_ref(g)
                    node = golden_node_id(gid)
                    print(f"[pin] golden {gid} landed on node: {node}")
                    if node != FORK_NODE_ID:
                        wrong.append((gid, node))
                if wrong:
                    raise RuntimeError(
                        f"{len(wrong)} golden(s) landed off the fork-capable node "
                        f"{FORK_NODE_ID}: {wrong}; aborting before fan-out")
        finally:
            if pinned:
                uncordon_others()

    print(f"[scale] {args.total} forked episodes @ concurrency={args.concurrency} "
          f"over {args.goldens} golden(s) on {FORK_NODE_ID}…")
    report = eng.run_scale(
        world, scripted_agent,
        total=args.total, concurrency=args.concurrency, goldens=args.goldens,
        before_build=before_build, after_build=after_build,
    )
    report.print_summary()

    # Surface the most common failure reason, if any (the summary omits it).
    bad = [r for r in report.results if r.outcome != "OK"]
    if bad:
        from collections import Counter
        reasons = Counter((r.outcome, (r.error or "")[:120]) for r in bad)
        print("\n[scale] non-OK breakdown:")
        for (outcome, err), n in reasons.most_common(5):
            print(f"  {n}x {outcome}: {err}")

    s = report.summary()
    print(f"\n[run] wrote {os.path.join(eng.results_dir, 'report.json')}")
    return 0 if (s["success_rate"] or 0) >= 0.95 else 1


if __name__ == "__main__":
    raise SystemExit(main())
