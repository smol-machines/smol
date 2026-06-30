"""Reusable RL-rollout harness for smolcloud (smolfleet).

This library turns the smol SDK's *fork-from-golden* primitive into an
RL-style rollout engine: build a reproducible environment ("world") once as a
forkable **golden** machine, then fork thousands of copy-on-write **clones**,
run an agent episode in each, grade the resulting state with a binary rubric,
and tear the clone down. It collects the latency/throughput/success metrics you
need to show the system holds up at scale.

Why fork: a clone inherits the golden's *warm* RAM and its raw disks via
copy-on-write, so each episode starts from a running, byte-identical VM in ~tens
of ms-to-~1s (cloud round-trip) instead of a cold boot. That warm-VM reuse is
the win — no per-episode cold boot, no image pull, no VM cold-start.

KEY FORK-PATH FACT (verified on prod — see README "Fork-survivability"): a clone
inherits the golden's warm RAM + raw CoW disks, but NOT the golden's *container
workload overlay*. That overlay is keyed by machine name and re-derived fresh for
the clone at exec time, so anything you `pip install` or `write_file` onto the
GOLDEN is invisible to a CLONE's `exec`. Build worlds that depend only on the
base image and drive per-episode state through `exec` (see world.py / agents.py).

KEY SCALING FACT — fork is node-local. A clone runs on the *same worker* as its
golden, so one golden fans out only to one node's capacity, and forks from a
single golden serialize (each fork briefly freezes the golden). To use the whole
fleet AND raise fork throughput you build N goldens (the scheduler spreads them
across nodes) and fork each locally. `RolloutEngine.run_scale(..., goldens=N)`
does exactly this; `run_episode` retries transient fork timeouts.

The SDK is synchronous; we drive concurrency with a thread pool (the cloud
transport is HTTP/urllib, which releases the GIL during I/O).

Usage sketch:

    from smol import ConnectOptions
    from harness import RolloutEngine
    from world import build_world
    from agents import scripted_agent

    eng = RolloutEngine(ConnectOptions(target="cloud"))   # reads SMOL_CLOUD_TOKEN
    report = eng.run_scale(build_world(), scripted_agent, total=200, concurrency=32, goldens=4)
    report.print_summary()

Cleanup is belt-and-suspenders: every machine id (goldens + clones) is tracked
in a file and deleted on exit, even on crash or Ctrl-C, so a scale run never
leaks billable machines on the shared fleet.
"""

from __future__ import annotations

import atexit
import json
import os
import signal
import statistics
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import Callable, Optional

from smol import (
    ConnectOptions,
    Machine,
    MachineConfig,
    ResourceSpec,
)

# An agent is a callable run inside a freshly-forked clone. It receives the
# clone Machine handle, the World, and the episode index, and does whatever an
# RL policy would do (exec commands, read/write files). Its return value is
# attached to the rollout for the rubric to inspect; it may also just mutate
# guest state and return None (the rubric reads the guest filesystem directly).
Agent = Callable[["Machine", "World", int], object]


def machine_ref(m: "Machine") -> str:
    """The stable, reconnectable id for a Machine.

    The cloud transport keys every call (exec, delete, connect) off the
    ``mach-…`` id, but the SDK's public ``.name`` returns the *friendly* name,
    which the API won't resolve. Prefer a public ``.id`` if the SDK grows one
    (a worthwhile addition), else read the transport's id, else fall back to
    name (correct for the local target). This is what we persist for crash-safe
    teardown so a scale run never leaks machines on the shared fleet.
    """
    pub = getattr(m, "id", None)
    if isinstance(pub, str) and pub:
        return pub
    t = getattr(m, "_t", None)
    tid = getattr(t, "_id", None)
    return tid or m.name


@dataclass
class Criterion:
    """One binary rubric check (mirrors apex-agents' binary-rubric grading)."""

    id: str
    description: str
    # check(clone, world, idx, agent_return) -> bool. Runs against the live
    # clone after the episode, so it can exec or read_file to inspect state.
    check: Callable[["Machine", "World", int, object], bool]


@dataclass
class Rubric:
    """A set of binary criteria. Reward = fraction of criteria satisfied."""

    criteria: list[Criterion]

    def grade(self, clone: Machine, world: "World", idx: int, agent_return: object) -> "Grade":
        results: dict[str, bool] = {}
        for c in self.criteria:
            try:
                results[c.id] = bool(c.check(clone, world, idx, agent_return))
            except Exception as e:  # a check that errors counts as failed
                results[c.id] = False
                results[f"{c.id}__error"] = True  # type: ignore[assignment]
        passed = sum(1 for k, v in results.items() if not k.endswith("__error") and v)
        total = len(self.criteria)
        return Grade(results=results, passed=passed, total=total,
                     reward=(passed / total) if total else 0.0)


@dataclass
class Grade:
    results: dict[str, bool]
    passed: int
    total: int
    reward: float


@dataclass
class World:
    """A reproducible agentic environment, modeled on an apex-agents 'world'.

    seed_files / setup_cmds run once on the GOLDEN. IMPORTANT: on the cloud fork
    path these do NOT reach a clone's `exec` (the container overlay is re-keyed
    per machine — see the module docstring), so they're useful for preparing the
    golden itself but a clone must materialize its own per-episode state via
    `exec`. They remain in the API because they're correct for non-fork machines
    and for any future engine that stacks the golden overlay under clones.
    """

    name: str
    image: str
    instruction: str
    rubric: Rubric
    seed_files: dict[str, str] = field(default_factory=dict)
    setup_cmds: list[list[str]] = field(default_factory=list)
    cpus: int = 1
    memory_mb: int = 512
    # Network is needed on the golden only if setup_cmds pull deps. Clones
    # inherit it but a well-formed world does no network during episodes.
    golden_network: bool = True


@dataclass
class RolloutResult:
    idx: int
    golden_id: str
    outcome: str            # OK | CAP | FAIL | ERROR
    fork_ms: Optional[float] = None
    episode_ms: Optional[float] = None
    grade_ms: Optional[float] = None
    reward: Optional[float] = None
    criteria: Optional[dict] = None
    error: Optional[str] = None


def _pct(xs: list[float], p: float) -> Optional[float]:
    if not xs:
        return None
    s = sorted(xs)
    k = max(0, min(len(s) - 1, int(round((p / 100.0) * (len(s) - 1)))))
    return s[k]


@dataclass
class ScaleReport:
    world: str
    total: int
    concurrency: int
    goldens: int
    wall_seconds: float
    results: list[RolloutResult]
    golden_build_seconds: list[float] = field(default_factory=list)

    @property
    def ok(self) -> list[RolloutResult]:
        return [r for r in self.results if r.outcome == "OK"]

    def summary(self) -> dict:
        ok = self.ok
        forks = [r.fork_ms for r in self.results if r.fork_ms is not None]
        eps = [r.episode_ms for r in ok if r.episode_ms is not None]
        rewards = [r.reward for r in ok if r.reward is not None]
        counts: dict[str, int] = {}
        for r in self.results:
            counts[r.outcome] = counts.get(r.outcome, 0) + 1
        return {
            "world": self.world,
            "total_rollouts": self.total,
            "concurrency": self.concurrency,
            "goldens": self.goldens,
            "wall_seconds": round(self.wall_seconds, 2),
            "throughput_rollouts_per_sec": round(len(ok) / self.wall_seconds, 2) if self.wall_seconds else None,
            "outcomes": counts,
            "success_rate": round(len(ok) / self.total, 4) if self.total else None,
            "mean_reward": round(statistics.fmean(rewards), 4) if rewards else None,
            "fully_solved_rate": round(sum(1 for x in rewards if x >= 1.0) / len(ok), 4) if ok else None,
            "fork_ms": {
                "p50": round(_pct(forks, 50), 1) if forks else None,
                "p95": round(_pct(forks, 95), 1) if forks else None,
                "p99": round(_pct(forks, 99), 1) if forks else None,
                "max": round(max(forks), 1) if forks else None,
            },
            "episode_ms": {
                "p50": round(_pct(eps, 50), 1) if eps else None,
                "p95": round(_pct(eps, 95), 1) if eps else None,
            },
            "golden_build_seconds": [round(s, 2) for s in self.golden_build_seconds],
        }

    def print_summary(self) -> None:
        s = self.summary()
        print("\n=== smolcloud RL-rollout scale report ===")
        print(json.dumps(s, indent=2))

    def to_json(self, path: str) -> None:
        with open(path, "w") as f:
            json.dump({"summary": self.summary(),
                       "rollouts": [r.__dict__ for r in self.results]}, f, indent=2)


class _MachineRegistry:
    """Tracks every created machine id and guarantees teardown on exit.

    Persisted to disk so a hard crash still leaves a deletable record, and
    deleted via atexit + SIGINT/SIGTERM so a scale run never leaks machines on
    the shared fleet.
    """

    def __init__(self, conn: ConnectOptions, path: str):
        self._conn = conn
        self._path = path
        self._ids: set[str] = set()
        self._lock = threading.Lock()
        self._installed = False

    def add(self, mid: str) -> None:
        with self._lock:
            self._ids.add(mid)
            with open(self._path, "a") as f:
                f.write(mid + "\n")

    def discard(self, mid: str) -> None:
        with self._lock:
            self._ids.discard(mid)

    def install_handlers(self) -> None:
        if self._installed:
            return
        self._installed = True
        atexit.register(self.delete_all)
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                signal.signal(sig, lambda *_: (self.delete_all(), os._exit(130)))
            except ValueError:
                pass  # not in main thread

    def delete_all(self) -> None:
        with self._lock:
            ids = list(self._ids)
        for mid in ids:
            try:
                Machine.connect(mid, self._conn).delete()
            except Exception:
                pass
            self.discard(mid)


class RolloutEngine:
    """Builds goldens and runs graded rollouts at concurrency on smolcloud."""

    def __init__(self, conn: Optional[ConnectOptions] = None, results_dir: Optional[str] = None):
        self.conn = conn or ConnectOptions(target="cloud")
        self.results_dir = results_dir or os.path.join(
            os.getcwd(), f"rl-run-{int(time.time())}")
        os.makedirs(self.results_dir, exist_ok=True)
        # Unique per-run token so machine names never collide across reruns.
        import uuid
        self.run_token = uuid.uuid4().hex[:6]
        self.registry = _MachineRegistry(self.conn, os.path.join(self.results_dir, "machine-ids.txt"))
        self.registry.install_handlers()
        # The id-registry can't catch a clone whose id we never received — e.g. a
        # fork that times out CLIENT-side but succeeds server-side. Every machine
        # we create carries run_token in its name, so a name-token sweep reaps
        # those orphans too. Registered last so it runs after the id-teardown.
        atexit.register(self.sweep_orphans)

    # --- crash-safe orphan sweep (cloud only) ----------------------------

    def _cloud_creds(self):
        """Resolve (base_url, api_key) the same way the SDK's cloud transport
        does, or return None for a non-cloud target / missing key."""
        if getattr(self.conn, "target", None) != "cloud":
            return None
        api_key = self.conn.api_key or os.environ.get("SMOL_CLOUD_TOKEN")
        if not api_key:
            return None
        base_url = (self.conn.base_url or os.environ.get("SMOL_CLOUD_URL")
                    or "https://api.smolmachines.com").rstrip("/")
        return base_url, api_key

    def sweep_orphans(self, token: Optional[str] = None) -> int:
        """Delete this run's orphan machines: those whose name carries our token
        but whose id the registry never tracked.

        The id we never received (a fork that times out CLIENT-side but succeeds
        server-side) can only be found by name. Crucially we spare machines the
        registry IS tracking — active goldens + in-flight clones also carry the
        token — so this is safe to call PERIODICALLY mid-run, not just at the end,
        which keeps untracked orphans from accumulating against the machine cap.

        Best-effort and idempotent; returns the number deleted. No-op off cloud.
        """
        creds = self._cloud_creds()
        if creds is None:
            return 0
        tok = token or self.run_token
        try:
            from smol.transport import _cloud_fetch
        except Exception:
            return 0
        base_url, api_key = creds
        try:
            listing = _cloud_fetch(base_url, api_key, "GET", "/v1/machines")
        except Exception:
            return 0
        machines = listing.get("machines", listing) if isinstance(listing, dict) else listing
        with self.registry._lock:
            tracked = set(self.registry._ids)
        deleted = 0
        for m in machines or []:
            name = (m.get("name") or "") if isinstance(m, dict) else ""
            mid = m.get("id") if isinstance(m, dict) else None
            if mid and tok in name and mid not in tracked:
                try:
                    _cloud_fetch(base_url, api_key, "DELETE", f"/v1/machines/{mid}")
                    deleted += 1
                except Exception:
                    pass
        return deleted

    # --- golden construction ---------------------------------------------

    def build_golden(self, world: World, name: str) -> Machine:
        """Create one forkable golden: boot, seed files, run setup. Returns the
        running golden ready to be forked. Raises on failure."""
        golden = Machine.create(
            MachineConfig(
                name=name,
                image=world.image,
                forkable=True,
                resources=ResourceSpec(
                    cpus=world.cpus,
                    memory_mb=world.memory_mb,
                    network=world.golden_network,
                ),
            ),
            self.conn,
        )
        self.registry.add(machine_ref(golden))
        # Prepare the golden's own filesystem. NOTE: on the cloud fork path these
        # do NOT propagate to a clone's exec (overlay re-keyed per machine) — see
        # the module docstring; clones materialize their own state via exec.
        for path, content in world.seed_files.items():
            golden.write_file(path, content)
        for cmd in world.setup_cmds:
            res = golden.exec(cmd, _setup_opts())
            if res.exit_code != 0:
                raise RuntimeError(
                    f"golden setup failed: {' '.join(cmd)} -> exit {res.exit_code}\n{res.stderr[:500]}")
        return golden

    # --- a single graded episode -----------------------------------------

    def run_episode(self, golden: Machine, world: World, agent: Agent, idx: int,
                    fork_retries: int = 2) -> RolloutResult:
        clone = None
        try:
            t0 = time.perf_counter()
            # Forking from one golden serializes (the golden freezes per fork), so
            # under concurrency a fork can transiently time out / contend. Retry a
            # few times with backoff before giving up — but never retry a capacity
            # rejection (that needs a freed slot, which the pool provides as other
            # clones finish, not an immediate re-fork).
            clone, fork_err, attempts = None, None, 0
            while attempts <= fork_retries:
                attempts += 1
                try:
                    clone = golden.fork(f"{world.name}-ep{idx}-a{attempts}-{self.run_token}")
                    break
                except Exception as e:
                    fork_err = e
                    msg = str(e).lower()
                    if "concurren" in msg or "capacity" in msg or "limit" in msg:
                        return RolloutResult(idx=idx, golden_id=machine_ref(golden),
                                             outcome="CAP", error=str(e)[:300])
                    if attempts <= fork_retries:
                        time.sleep(0.5 * attempts)
            if clone is None:
                return RolloutResult(idx=idx, golden_id=machine_ref(golden),
                                     outcome="FAIL", error=str(fork_err)[:300])
            fork_ms = (time.perf_counter() - t0) * 1000.0
            self.registry.add(machine_ref(clone))

            t1 = time.perf_counter()
            agent_return = agent(clone, world, idx)
            episode_ms = (time.perf_counter() - t1) * 1000.0

            t2 = time.perf_counter()
            grade = world.rubric.grade(clone, world, idx, agent_return)
            grade_ms = (time.perf_counter() - t2) * 1000.0

            return RolloutResult(
                idx=idx, golden_id=machine_ref(golden), outcome="OK",
                fork_ms=fork_ms, episode_ms=episode_ms, grade_ms=grade_ms,
                reward=grade.reward, criteria=grade.results,
            )
        except Exception as e:
            return RolloutResult(idx=idx, golden_id=machine_ref(golden), outcome="ERROR", error=str(e)[:300])
        finally:
            if clone is not None:
                try:
                    clone.delete()
                    self.registry.discard(machine_ref(clone))
                except Exception:
                    pass  # registry teardown will retry on exit

    # --- the scale run ----------------------------------------------------

    def run_scale(self, world: World, agent: Agent, total: int,
                  concurrency: int, goldens: int = 1,
                  before_build: Optional[Callable[[], None]] = None,
                  after_build: Optional[Callable[[list], None]] = None) -> ScaleReport:
        """Build `goldens` goldens (spread across nodes by the scheduler), then
        run `total` graded episodes across them at `concurrency`, round-robining
        forks over the goldens so the load fans out across the fleet.

        Placement hooks (both optional) let a caller control *where* goldens land
        without re-implementing the scale loop:
          - before_build()        — runs once before any golden is created (e.g.
                                     cordon nodes so goldens land on a target).
          - after_build(goldens)  — runs once after all goldens are built, before
                                     forking (e.g. verify placement, un-cordon).
        If after_build raises, the goldens are torn down and the run aborts — so a
        failed placement check never fans out onto the wrong node.
        """
        print(f"[golden] building {goldens} golden(s) for world '{world.name}' "
              f"(image={world.image}, {world.cpus}cpu/{world.memory_mb}MB)…")
        if before_build:
            before_build()
        built: list[Machine] = []
        build_secs: list[float] = []
        try:
            for g in range(goldens):
                tg = time.perf_counter()
                golden = self.build_golden(world, f"{world.name}-g{g}-{self.run_token}")
                build_secs.append(time.perf_counter() - tg)
                built.append(golden)
                print(f"[golden] golden{g} ready: {machine_ref(golden)} ({build_secs[-1]:.1f}s)")
            if after_build:
                after_build(built)
        except Exception:
            for golden in built:
                try:
                    golden.delete()
                    self.registry.discard(machine_ref(golden))
                except Exception:
                    pass
            raise

        print(f"[scale] running {total} rollouts @ concurrency={concurrency} "
              f"over {goldens} golden(s)…")
        results: list[RolloutResult] = []
        wall0 = time.perf_counter()
        with ThreadPoolExecutor(max_workers=concurrency) as pool:
            futs = {
                pool.submit(self.run_episode, built[i % goldens], world, agent, i): i
                for i in range(total)
            }
            done = 0
            checkpoint = max(1, total // 20)
            for fut in as_completed(futs):
                results.append(fut.result())
                done += 1
                if done % checkpoint == 0 or done == total:
                    ok = sum(1 for r in results if r.outcome == "OK")
                    print(f"[scale]   {done}/{total} done ({ok} OK)")
                    # Periodically reap fork-timeout orphans the id-registry never
                    # saw. sweep_orphans() spares everything still tracked (live
                    # goldens + in-flight clones), so this only deletes true
                    # orphans — keeping the active-machine count bounded well under
                    # the tenant cap on long, high-concurrency runs.
                    if done != total:
                        swept = self.sweep_orphans()
                        if swept:
                            print(f"[scale]   (swept {swept} mid-run orphan(s))")
        wall = time.perf_counter() - wall0

        results.sort(key=lambda r: r.idx)
        report = ScaleReport(world=world.name, total=total, concurrency=concurrency,
                             goldens=goldens, wall_seconds=wall, results=results,
                             golden_build_seconds=build_secs)

        # Tear down goldens (clones already deleted per-episode).
        for golden in built:
            try:
                golden.delete()
                self.registry.discard(machine_ref(golden))
            except Exception:
                pass
        # Reap any orphan clones the id-registry missed (fork-timeout race).
        swept = self.sweep_orphans()
        if swept:
            print(f"[cleanup] swept {swept} orphan machine(s) by run-token")
        report.to_json(os.path.join(self.results_dir, "report.json"))
        return report


def _setup_opts():
    from smol import ExecOptions
    return ExecOptions(timeout=300)
