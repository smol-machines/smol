/** The public `Machine` — an isolated microVM, local (embedded) or cloud.
 *
 *  The backend is chosen by `ConnectOptions`:
 *    - default / `{ target: 'local' }` → embedded engine, no server.
 *    - `{ target: 'cloud', apiKey }` (or `SMOL_CLOUD_TOKEN`) → smolfleet cloud.
 *
 *  Both go through the same `Transport`, so calling code is identical; features
 *  a given backend lacks throw `NotSupportedError`.
 */

import { randomUUID } from "node:crypto";
import { ExecutionError } from "./errors";
import {
  makeTransport,
  connectTransport,
  type RawExec,
  type Transport,
} from "./transport";
import type {
  AssignOptions,
  ConnectOptions,
  ExecEvent,
  ExecOptions,
  ExecResult,
  ForkBatchOptions,
  ImageInfo,
  MachineConfig,
  PortEndpoint,
  PortSpec,
  WaitReadyOptions,
} from "./types";

function makeExecResult(r: RawExec): ExecResult {
  const success = r.exitCode === 0;
  return {
    exitCode: r.exitCode,
    stdout: r.stdout,
    stderr: r.stderr,
    // Cloud-only truncation flags (output capped at 1 MiB); the local engine
    // streams unbounded, so absent means false.
    stdoutTruncated: r.stdoutTruncated ?? false,
    stderrTruncated: r.stderrTruncated ?? false,
    // Byte-exact output. The cloud transport decodes it from base64; the local
    // transport has no bytes, so fall back to the UTF-8 encoding of the text.
    stdoutBytes: r.stdoutBytes ?? new TextEncoder().encode(r.stdout),
    stderrBytes: r.stderrBytes ?? new TextEncoder().encode(r.stderr),
    success,
    output: r.stdout + r.stderr,
    assertSuccess() {
      if (!success) throw new ExecutionError(r.exitCode, r.stdout, r.stderr);
    },
  };
}

export class Machine {
  private constructor(private readonly transport: Transport) {}

  /**
   * Create and start a machine. On the cloud target, this does not return until
   * the machine is ready for work: the guest agent is reachable and any
   * published port is accepting connections. A cloud state of `"started"`
   * alone means only that the VM process launched.
   *
   * @param config  machine configuration (a name is generated if omitted; `image`
   *                is the base image — required for cloud, optional for local)
   * @param conn    backend selection (local embedded by default)
   */
  static async create(
    config: MachineConfig = {},
    conn: ConnectOptions = {},
  ): Promise<Machine> {
    return new Machine(await makeTransport(config, conn));
  }

  /**
   * Attach to an EXISTING machine without creating a new one — to drive a
   * machine made elsewhere (another process, the console, the REST API).
   *  - local (default): re-opens a persisted machine by NAME, starting it if
   *    stopped — pairs with `Machine.create({ name, … }, …)` + `persistent`.
   *  - cloud: looks up the machine by id; throws if it doesn't exist.
   *    This does not wait for readiness; call `waitUntilReady()` before `exec`,
   *    using a connect endpoint, or expecting the workload to respond.
   *
   * @param id    local machine name, or cloud machine id (`mach-…`)
   * @param conn  backend selection (local by default; cloud via `{ target: 'cloud', apiKey }` or `SMOL_CLOUD_TOKEN`)
   */
  static async connect(
    id: string,
    conn: ConnectOptions = {},
  ): Promise<Machine> {
    return new Machine(await connectTransport(id, conn));
  }

  /** The machine's name / identifier. */
  get name(): string {
    return this.transport.name;
  }

  /** The machine's id — the cloud `mach-…` id (or the name on local). Handy right
   *  after `create()`/`fork()` so you don't have to list the tenant to find the
   *  machine you just made. */
  get id(): string {
    return this.transport.machineId;
  }

  /** Current lifecycle state. On cloud, `"started"` means only that the VM
   *  process launched; it does not mean the guest or workload is ready. Use
   *  `ready()` or `waitUntilReady()` before doing work. */
  state(): Promise<string> {
    return this.transport.state();
  }

  /** Whether the machine is READY to do work. `state()` becoming "started"
   *  means only that the VM process launched — the guest is still booting and
   *  is NOT yet usable. `ready` becomes true once the in-VM agent is reachable
   *  (an `exec`/`connect` will succeed) and any published port accepts
   *  connections. Gate on this, not `state`, before driving the machine.
   *  (cloud; the local target reports ready once running.) */
  ready(): Promise<boolean> {
    return this.transport.ready();
  }

  /** When the machine first became ready (RFC3339), or `null` if not yet ready. */
  readyAt(): Promise<string | null> {
    return this.transport.readyAt();
  }

  /** Block until the machine is `ready` (or throw on a failed/stopped state or
   *  timeout). `create()` already waits for readiness, so this is for machines
   *  attached via `Machine.connect(...)`, or to re-assert readiness before use.
   *  Poll ceiling and interval are configurable (defaults: 120s / 1s). */
  waitUntilReady(opts?: WaitReadyOptions): Promise<void> {
    return this.transport.waitUntilReady(opts);
  }

  /** An authenticated endpoint (URL + headers) to reach a PUBLISHED guest port
   *  through the control plane's connect bridge — no Cloudflare/localhost.run
   *  tunnel, no public exposure, no egress allow-list. Have the in-VM worker
   *  LISTEN on the port and connect *inbound*: plug `wsUrl` into a WebSocket
   *  client (passing `headers`) or `httpUrl` into `fetch`. The machine must
   *  publish the port (`ports: [{ guest }]` at create). (cloud) */
  endpoint(port: number, path?: string): PortEndpoint {
    return this.transport.endpoint(port, path);
  }

  /** Convenience: an authenticated HTTP request to a published guest port via
   *  the connect bridge. Returns the raw `fetch` `Response`. (cloud)
   *
   *  @param port  a published GUEST port the in-VM service listens on
   *  @param path  optional path on that service (e.g. `"healthz"`)
   *  @param init  standard `fetch` init; its headers merge over the auth header */
  fetch(port: number, path?: string, init?: RequestInit): Promise<Response> {
    const e = this.transport.endpoint(port, path);
    return fetch(e.httpUrl, {
      ...init,
      headers: {
        ...e.headers,
        ...((init?.headers as Record<string, string> | undefined) ?? {}),
      },
    });
  }

  /** Public ingress URL for the machine's first published port (cloud).
   *  `null` until the machine is started with an allocated host port, for
   *  machines with no published port, or on the local target (no public
   *  ingress). Reach the deployed app over HTTPS at the returned URL. */
  url(): Promise<string | null> {
    return this.transport.url();
  }

  /** Execute a command directly in the machine. */
  async exec(command: string[], opts?: ExecOptions): Promise<ExecResult> {
    return makeExecResult(await this.transport.exec(command, opts));
  }

  /** Pull an image (if needed) and run a command in a container of it. (local) */
  async run(
    image: string,
    command: string[],
    opts?: ExecOptions,
  ): Promise<ExecResult> {
    return makeExecResult(await this.transport.run(image, command, opts));
  }

  /** Execute a command, yielding stdout/stderr/exit events. (local) */
  execStream(command: string[], opts?: ExecOptions): AsyncGenerator<ExecEvent> {
    return this.transport.execStream(command, opts);
  }

  /** Read a file from the machine. */
  readFile(path: string): Promise<Buffer> {
    return this.transport.readFile(path);
  }

  /** Write a file into the machine. */
  writeFile(
    path: string,
    data: string | Uint8Array,
    mode?: number,
  ): Promise<void> {
    const buf =
      typeof data === "string" ? Buffer.from(data) : Buffer.from(data);
    return this.transport.writeFile(path, buf, mode);
  }

  /** Pull an OCI image into the machine's storage. (local) */
  pullImage(image: string): Promise<ImageInfo> {
    return this.transport.pullImage(image);
  }

  /** List cached OCI images. (local) */
  listImages(): Promise<ImageInfo[]> {
    return this.transport.listImages();
  }

  /** Stop the machine. */
  stop(): Promise<void> {
    return this.transport.stop();
  }

  /** Start (resume) a stopped machine, awaiting until its agent is ready. The
   *  counterpart to `stop()` — disk state is preserved across a stop/start, so
   *  this is a cheap pause/resume. */
  start(): Promise<void> {
    return this.transport.start();
  }

  /** Stop the machine and delete its storage. */
  delete(): Promise<void> {
    return this.transport.delete();
  }

  /** Fork this running, forkable machine into a new clone via copy-on-write live
   *  RAM + disks (cloud target). The clone inherits the golden's warm in-memory
   *  state and runs on the same node; forks are fast (~tens of ms) and repeatable
   *  from one golden — the basis for RL rollout branching and instant episode
   *  reset. The golden must have been created with `MachineConfig({ forkable: true })`.
   *
   *  @param name  name for the new clone machine.
   *  @param ports optional pinned inbound port forwards (`{ host, guest }`); by
   *               default the node allocates fresh host ports so clones don't collide.
   *  @returns a `Machine` handle to the running clone. */
  async fork(name: string, ports?: PortSpec[]): Promise<Machine> {
    return new Machine(await this.transport.fork(name, ports));
  }

  /** Fork this forkable machine into MANY clones in one call — the RL fan-out
   *  primitive (GRPO group sampling / eval-task fan-out). Each clone is a live-RAM
   *  CoW fork off this golden, like `fork()`. On the cloud target the batch is
   *  transactional (all-or-nothing: if any clone fails, the whole batch is rolled
   *  back), so you get all N branches or none. Provide either `count` (clones
   *  auto-named `{namePrefix}-{n}`) or explicit `names`. Requires this machine to
   *  be `forkable`.
   *
   *  @param opts  batch size (`count` or `names`), optional `namePrefix`/`ports`
   *  @returns the clones, in request order
   *
   *  ```ts
   *  const clones = await golden.forkBatch({ count: 32, namePrefix: "rollout" });
   *  await Promise.all(clones.map((c) => c.exec(["python", "rollout.py"])));
   *  ``` */
  async forkBatch(opts: ForkBatchOptions): Promise<Machine[]> {
    const clones = await this.transport.forkBatch(opts);
    return clones.map((t) => new Machine(t));
  }

  /** Assign an RL episode: fork + provision a clone of this forkable golden under
   *  an idempotent lease, returned only once fully installed — with lifecycle
   *  control (`Episode.heartbeat()` / `Episode.complete()`). The scheduler's "give
   *  me one clean worker for this task, exactly once" call. **Cloud target only.**
   *
   *  Idempotent on `opts.leaseId`: a retried assign with the same key returns the
   *  SAME episode instead of a second fork. `task` is staged into the clone at
   *  `/run/smol-task.json`, `secrets` at `/run/smol-secrets.json`, and `files` at
   *  their paths — all before the episode is handed back. Always `complete()` the
   *  episode (e.g. in a `finally`) so its clone is reclaimed.
   *
   *  ```ts
   *  const ep = await golden.assign({ leaseId: "task-42", task: { seed: 7 } });
   *  try {
   *    await ep.exec(["python", "rollout.py"]);
   *  } finally {
   *    await ep.complete("done");
   *  }
   *  ``` */
  async assign(opts: AssignOptions): Promise<Episode> {
    const { transport, leaseId, ownerToken } = await this.transport.assign(opts);
    return new Episode(new Machine(transport), leaseId, ownerToken, this.transport);
  }

  /** Score this machine's state WITHOUT mutating it: fork an ephemeral clone,
   *  run the grader command in the clone, then destroy it. The result (exit code
   *  / stdout) is your reward signal; this machine is untouched. This is the RL
   *  "reward judging without side effects" primitive — grade the end of a rollout,
   *  or run a verifier — as one call, with the throwaway clone always cleaned up
   *  even if the grader errors. Requires this machine to be `forkable` (like
   *  `fork()`); a clone name is generated unless you pass one.
   *
   *  @param command  the grader/verifier argv to run in the clone
   *  @param opts      exec options (env, cwd, timeout, …)
   *  @param name      optional clone name (default: a generated unique name)
   *  @returns the grader's `ExecResult` (use `.success` for pass/fail, `.stdout` for a score) */
  async rewardFork(
    command: string[],
    opts?: ExecOptions,
    name?: string,
  ): Promise<ExecResult> {
    const cloneName = name ?? `${this.name}-rf-${randomUUID().slice(0, 8)}`;
    const clone = await this.fork(cloneName);
    try {
      return await clone.exec(command, opts);
    } finally {
      // Best-effort teardown — never mask the grader's result/error.
      await clone.delete().catch(() => {});
    }
  }
}

/** A leased RL episode: a freshly-provisioned clone plus its lease. Created by
 *  `Machine.assign()`. Always `complete()` it (e.g. in a `finally`) so the clone
 *  is reclaimed — the RL "one clean worker per task, guaranteed reclaimed"
 *  pattern. Cloud target only. */
export class Episode {
  readonly #machine: Machine;
  readonly leaseId: string;
  readonly #ownerToken: string;
  readonly #leaseTransport: Transport;
  #completed = false;

  constructor(
    machine: Machine,
    leaseId: string,
    ownerToken: string,
    leaseTransport: Transport,
  ) {
    this.#machine = machine;
    this.leaseId = leaseId;
    this.#ownerToken = ownerToken;
    this.#leaseTransport = leaseTransport;
  }

  /** The episode's provisioned clone. */
  get machine(): Machine {
    return this.#machine;
  }

  /** Run a command in the episode's machine (shortcut for `.machine.exec`). */
  async exec(command: string[], opts?: ExecOptions): Promise<ExecResult> {
    return this.#machine.exec(command, opts);
  }

  /** Keep the lease alive. Call within the `heartbeatSecs` cadence you requested,
   *  or the episode is reclaimed as `heartbeat_lost`. */
  async heartbeat(): Promise<void> {
    await this.#leaseTransport.heartbeatLease(this.leaseId, this.#ownerToken);
  }

  /** Finish the episode with a typed termination reason (e.g. `done`,
   *  `agent_failed`, `infra_failed`, `cancelled`) and tear its clone down.
   *  Optionally record a `score` (per-task reward / pass-rate) and an arbitrary
   *  JSON `result` — the "did it improve?" number, read back via `status()`.
   *  Idempotent — a second call is a no-op. */
  async complete(
    reason = "done",
    opts?: { score?: number; result?: unknown },
  ): Promise<void> {
    if (this.#completed) return;
    await this.#leaseTransport.completeLease(
      this.leaseId,
      this.#ownerToken,
      reason,
      opts?.score,
      opts?.result,
    );
    this.#completed = true;
  }

  /** Read this lease's current status and, once completed, its outcome (`state`,
   *  `reason`, `score`, `result`) — how a trainer collects the per-task score. */
  async status(): Promise<Record<string, unknown>> {
    return this.#leaseTransport.getLease(this.leaseId);
  }
}
