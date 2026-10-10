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
import { resolve as resolvePath } from "node:path";
import { ExecutionError, NotSupportedError, wrapNativeError } from "./errors";
import { getNapiMachine } from "./native";
import { localAvailability, type LocalAvailability } from "./availability";
import {
  makeTransport,
  connectTransport,
  listMachines,
  deleteCheckpointTransport,
  restoreCheckpointTransport,
  uploadCheckpoint,
  createCacheDisk,
  listCacheDisks,
  getCacheDisk,
  deleteCacheDisk,
  type CloudCheckpointInfo,
  type RawExec,
  type Transport,
} from "./transport";
import type {
  CacheDiskInfo,
  CreateCacheDiskOptions,
  PublishedCacheDisk,
  MachineResources,
  ResizeOptions,
  AssignOptions,
  BranchBatchOptions,
  BranchOptions,
  CheckpointOptions,
  ConnectOptions,
  ExecEvent,
  ExecOptions,
  ExecResult,
  ForkBatchOptions,
  ForkOptions,
  ImageInfo,
  ListOptions,
  MachineConfig,
  MachineSummary,
  NetworkPolicy,
  RestoreCheckpointOptions,
  StartOptions,
  ResumeOptions,
  MachineUsageReport,
  ShareLink,
  PortableCheckpointInfo,
  PortEndpoint,
  PortSpec,
  WaitReadyOptions,
} from "./types";

function makeExecResult(r: RawExec): ExecResult {
  const success = r.exitCode === 0;
  return {
    ...(r.pid !== undefined ? { pid: r.pid } : {}),
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
   * Create and start a machine. By default this waits for the guest agent and
   * all published ports. `waitForPorts: false` returns when the agent is
   * ready so `exec()` can start a service on a published port.
   * A cloud state of `"started"` alone means only that the VM process launched.
   *
   * @param config  machine configuration (a name is generated if omitted; `image`
   *                is the base image — required for cloud, optional for local)
   * @param conn    backend selection (local embedded by default)
   */
  static async create(
    config: MachineConfig = {},
    conn: ConnectOptions = {},
  ): Promise<Machine> {
    // The native/cloud transports still use the compatibility `forkable`
    // field. Normalize every accepted public spelling at this boundary.
    const branchable = config.branchable ?? config.forkable ?? config.checkpoint;
    if (branchable !== undefined) config = { ...config, forkable: branchable };
    return new Machine(await makeTransport(config, conn));
  }

  /**
   * Attach to an EXISTING machine without creating a new one — to drive a
   * machine made elsewhere (another process, the console, the REST API).
   *  - local (default): re-opens a persisted machine by NAME, starting it if
   *    stopped — pairs with `Machine.create({ name, … }, …)` + `persistent`.
   *    Set `conn.waitForPorts: false` to attach after agent readiness when a
   *    published service is not listening (for example, to delete it).
   *  - cloud: looks up the machine by id; throws if it doesn't exist.
   *    This does not wait for readiness by default. With `waitForPorts: false`,
   *    it waits for the guest agent so `exec()` can start a published service.
   *    Call `waitUntilReady()` before expecting the published service to respond.
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

  /**
   * List every machine the target knows about — including ones created by
   * other processes, or by this one before it last restarted. Locally that is
   * the engine's database (`smol machine ls` reads the same one); on the cloud,
   * the account's machines. Pass `labels` to keep only your own, then
   * `Machine.connect(name)` to pick one up.
   *
   * @param conn     backend selection, as for `connect`
   * @param options  `labels`: keep machines carrying every one of these
   */
  static async list(
    conn: ConnectOptions = {},
    options: ListOptions = {},
  ): Promise<MachineSummary[]> {
    return listMachines(conn, options);
  }

  /** Check that a cloud connection works: the key is accepted and the base URL
   *  answers with a machine list. Throws otherwise, including when a proxy or
   *  a wrong URL answers with something that is not a machine list. */
  static async probe(conn: ConnectOptions = {}): Promise<void> {
    if (conn.target === "local") throw new NotSupportedError("probe is cloud-only.");
    await listMachines({ ...conn, target: "cloud" });
  }

  /** Upload a `.checkpoint` file taken on this computer to the cloud, so it
   *  can be restored there by the returned id. Restoring the file with a cloud
   *  target uploads it for you. */
  static async uploadCheckpoint(
    path: string,
    conn: ConnectOptions = {},
    onProgress?: (sent: number, total: number) => void,
  ): Promise<CloudCheckpointInfo> {
    return uploadCheckpoint(path, conn, onProgress);
  }

  /** Delete a local `.smolcheckpoint` file or a durable cloud checkpoint by id.
   *  A cloud checkpoint that a machine was restored from, or that holds a paused
   *  machine's saved execution, is refused until that machine is gone. */
  static async deleteCheckpoint(checkpointId: string, conn?: ConnectOptions): Promise<void> {
    return deleteCheckpointTransport(checkpointId, conn);
  }

  /** Restore a local `.smolcheckpoint` path or durable cloud checkpoint id into
   * a ready, immediately forkable machine. With a cloud target, a local file is
   * uploaded first, so a machine checkpointed here resumes in the cloud. */
  static async restoreCheckpoint(
    checkpointId: string,
    name: string,
    conn?: ConnectOptions,
    options?: RestoreCheckpointOptions,
  ): Promise<Machine> {
    return new Machine(await restoreCheckpointTransport(checkpointId, name, conn, options));
  }

  /** Export a local stored checkpoint directory as one portable file. */
  static exportCheckpoint(source: string, output: string): number {
    try {
      return getNapiMachine().exportCheckpoint(resolvePath(source), resolvePath(output));
    } catch (error) {
      throw wrapNativeError(error);
    }
  }

  /** Whether this host can run local machines, answered without booting one.
   *  Checks the platform, that the local engine is installed, and the engine's
   *  own host requirements (`/dev/kvm` on Linux), reporting the same `code` a
   *  failing local `create()` would. Never throws. Use it to choose between a
   *  local machine and another sandbox up front. */
  static localAvailability(): LocalAvailability {
    return localAvailability();
  }

  /** Remove objects that no retained checkpoint in a local store references. */
  static pruneCheckpointStore(store: string): number {
    try {
      return getNapiMachine().pruneCheckpointStore(resolvePath(store));
    } catch (error) {
      throw wrapNativeError(error);
    }
  }

  /** Restore a local `.smolcheckpoint` path or durable cloud checkpoint id.
   * Alias of {@link restoreCheckpoint}. */
  static restore(
    checkpointId: string,
    name: string,
    conn?: ConnectOptions,
  ): Promise<Machine> {
    return Machine.restoreCheckpoint(checkpointId, name, conn);
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

  /** The machine's full cloud record, every field the control plane returns
   *  (env, readiness, ports, …). Cloud only. */
  info(): Promise<Record<string, unknown>> {
    if (!this.transport.info) throw new NotSupportedError("info is cloud-only.");
    return this.transport.info();
  }

  /** The last `tail` lines of the machine's console log. Cloud only. */
  logs(tail = 100): Promise<string> {
    if (!this.transport.logs) throw new NotSupportedError("logs is cloud-only.");
    return this.transport.logs(tail);
  }

  /** Export this stopped machine as a `.smolmachine` in your registry
   *  namespace, re-deployable anywhere. Distinct from a portable checkpoint,
   *  which also keeps live memory. Cloud only. */
  exportArtifact(): Promise<Record<string, unknown>> {
    if (!this.transport.exportArtifact) throw new NotSupportedError("exportArtifact is cloud-only.");
    return this.transport.exportArtifact();
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

  /** An endpoint (URL + any required headers) for a PUBLISHED guest port.
   *  Local machines resolve to their current localhost host-port mapping;
   *  cloud machines use the authenticated connect bridge. */
  endpoint(port: number, path?: string): PortEndpoint {
    return this.transport.endpoint(port, path);
  }

  /** Convenience HTTP request to a published guest port. Returns the raw
   *  `fetch` `Response` on local and cloud targets.
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

  /** Copy guest-local staged mounts back to their host directories without stopping. */
  sync(): Promise<void> {
    return this.transport.sync();
  }

  /** Replace this stopped machine's outbound network policy. A machine that
   *  resumes saved memory (restored or paused) keeps its network backend, so a
   *  policy that would need a different one is refused. (local) */
  setNetworkPolicy(policy: NetworkPolicy): Promise<void> {
    return this.transport.setNetworkPolicy(policy);
  }

  /** Stop the machine. */
  stop(): Promise<void> {
    return this.transport.stop();
  }

  /** Save RAM and disk durably, then stop at that execution boundary. */
  pause(): Promise<void> { return this.transport.pause(); }

  /** Resume saved execution in this machine. */
  resume(options: ResumeOptions = {}): Promise<void> {
    return this.transport.resume(options.waitUntilReady ?? true);
  }

  /** Boot a stopped machine with its disk state, not its previous RAM.
   *  The local handle reuses its interceptor binding or accepts a fresh one. */
  start(options: StartOptions = {}): Promise<void> {
    return this.transport.start(options.egressInterceptor, options.waitUntilReady ?? true);
  }

  /** Stop the machine and delete its storage. On the cloud target, pass
   *  `{ includeUsage: true }` to also get the fully settled usage + cost back
   *  in one call — the control plane takes a final metering sample before
   *  teardown, so nothing accrues after the returned report. */
  delete(): Promise<void>;
  delete(opts: { includeUsage: true }): Promise<MachineUsageReport>;
  delete(opts?: {
    includeUsage?: boolean;
  }): Promise<MachineUsageReport | void> {
    if (opts?.includeUsage) return this.transport.deleteWithUsage();
    return this.transport.delete();
  }

  /** Metered usage + cost for this machine (cloud target). Usage records
   *  survive deletion for 30 days. CPU/memory accrue via a rollup that samples
   *  every few minutes, so a mid-life read is a lower bound; the report is
   *  final once the machine stops or is deleted. */
  usage(): Promise<MachineUsageReport> {
    return this.transport.usage();
  }

  /** Mint an anonymous share link for this machine's published app (cloud
   *  target). Anyone holding the URL reaches the app without a smolmachines
   *  account, so treat it as a credential. Minting again replaces the previous
   *  token, and {@link unshare} revokes it. */
  share(): Promise<ShareLink> {
    return this.transport.share();
  }

  /** Revoke this machine's anonymous share link (cloud target). The existing
   *  URL immediately stops granting access. */
  unshare(): Promise<void> {
    return this.transport.unshare();
  }

  /** Publish this machine's cache disk, the version it started from plus
   *  everything it wrote, as its cache disk's next version (cloud target).
   *  Stop the machine first: a running machine is still writing it. */
  publishCacheDisk(): Promise<PublishedCacheDisk> {
    return this.transport.publishCacheDisk();
  }

  /** Capture this running checkpointable machine. Local capture requires an
   * output path; with `store`, it is a self-contained directory, otherwise a
   * `.smolcheckpoint` file. Cloud capture stores the artifact durably. */
  checkpoint(
    output?: string,
    options?: CheckpointOptions,
  ): Promise<PortableCheckpointInfo> {
    return this.transport.checkpoint(output, options);
  }

  /** List durable portable checkpoints captured from this machine. */
  checkpoints(): Promise<PortableCheckpointInfo[]> {
    return this.transport.checkpoints();
  }

  /** Add CPUs, RAM or disk to this running machine without rebooting it.
   *  Sizes are totals. Every target is checked before anything changes, then
   *  RAM, CPUs and disks are applied in that order. On the cloud the machine
   *  has one disk, set with `storageGb`, and memory and disk only grow. */
  resize(options: ResizeOptions): Promise<MachineResources> {
    return this.transport.resize(options);
  }

  /** Branch an independent child from this running source. The child inherits
   *  warm RAM and disk through copy-on-write while the source remains available
   *  for more branches. Create the source with `{ branchable: true }`.
   *
   *  @param name name for the child machine.
   *  @param options optional pinned ports and `branchable: true` when the child
   *                 must itself become a branch source. */
  branch(name: string, options?: PortSpec[] | BranchOptions): Promise<Machine> {
    return this.transport.fork(name, options).then((transport) => new Machine(transport));
  }

  /** @deprecated Use {@link branch}. */
  fork(name: string, options?: PortSpec[] | ForkOptions): Promise<Machine> {
    return this.branch(name, options);
  }

  /** Branch this source into MANY independent children in one call. On the
   *  cloud target the batch is transactional, so callers receive all children
   *  or none. Create the source with `{ branchable: true }`.
   *
   *  @param opts  batch size (`count` or `names`), optional `namePrefix`/`ports`
   *  @returns the children, in request order
   *
   *  ```ts
   *  const children = await source.branchBatch({ count: 32, namePrefix: "rollout" });
   *  await Promise.all(children.map((child) => child.exec(["python", "rollout.py"])));
   *  ``` */
  branchBatch(opts: BranchBatchOptions): Promise<Machine[]> {
    return this.transport
      .forkBatch(opts)
      .then((children) => children.map((transport) => new Machine(transport)));
  }

  /** @deprecated Use {@link branchBatch}. */
  async forkBatch(opts: ForkBatchOptions): Promise<Machine[]> {
    return this.branchBatch(opts);
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

/**
 * Cache disks: disk images many machines start from, each through its own
 * copy-on-write layer, published in immutable versions (cloud target).
 *
 * ```ts
 * const deps = await CacheDisk.create({ name: "deps" });           // v0: empty
 * const m = await Machine.create({ image: "node:22", cacheDisk: { cache: "deps" } });
 * await m.exec(["sh", "-c", "cd /cache && npm install"]);
 * await m.stop();
 * await m.publishCacheDisk();                                      // v1
 * // Every later machine with cacheDisk: { cache: "deps" } starts from v1.
 * ```
 */
export class CacheDisk {
  /** Create a cache disk; its version 0 is an empty filesystem. */
  static create(options: CreateCacheDiskOptions, conn: ConnectOptions = {}): Promise<CacheDiskInfo> {
    return createCacheDisk(options, conn);
  }

  /** Every cache disk in the account, versions newest first. */
  static list(conn: ConnectOptions = {}): Promise<CacheDiskInfo[]> {
    return listCacheDisks(conn);
  }

  /** One cache disk by id or name. */
  static get(idOrName: string, conn: ConnectOptions = {}): Promise<CacheDiskInfo> {
    return getCacheDisk(idOrName, conn);
  }

  /** Delete a cache disk and all its versions; refused while a machine uses it. */
  static delete(idOrName: string, conn: ConnectOptions = {}): Promise<void> {
    return deleteCacheDisk(idOrName, conn);
  }
}
