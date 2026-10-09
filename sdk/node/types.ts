/** Public types for the `smol` SDK. Backend-agnostic; mapped to the native
 *  addon (local) or, in a later phase, the cloud REST API. */

/** Lifecycle state of a machine. Cloud `"started"` means the VM process
 * launched, not that the guest agent or workload is ready; use `ready()` or
 * `waitUntilReady()` before doing work. */
export type MachineState = "created" | "started" | "running" | "stopped" | "pausing" | "paused";

/** CPU / memory / disk / network allocation for a machine. */
export interface ResourceSpec {
  /** Number of vCPUs. */
  cpus?: number;
  /** Memory in MB. */
  memoryMb?: number;
  /**
   * Enable unrestricted outbound network access (TSI). To allow only specific
   * destinations, set `allowHosts` or `allowCidrs` instead of this flag;
   * either one enables networking scoped to its list. Unset means off locally
   * and the control plane's default (open) on the cloud; `false` blocks egress
   * on both. A blocked cloud machine cannot pull an image the node has not
   * cached.
   */
  network?: boolean;
  /**
   * Scope egress to these CIDR ranges. Setting this (or `allowHosts`) enables
   * networking and restricts it to the listed CIDRs. Enforced on both the local
   * and cloud targets.
   */
  allowCidrs?: string[];
  /**
   * Scope egress to these hostnames and their subdomains (e.g.
   * `api.anthropic.com`). Setting this (or `allowCidrs`) enables networking and
   * restricts it to the listed hosts; names are enforced at DNS inside the
   * machine, so a host served from another domain (a CDN) needs its own entry.
   * Enforced on both the local and cloud targets.
   */
  allowHosts?: string[];
  /** Storage disk size in GB (default: 20). */
  storageGb?: number;
  /** Overlay disk size in GB (default: 10). */
  overlayGb?: number;
  /** Enable GPU acceleration (virtio-gpu/venus). Local target only. Default: false. */
  gpu?: boolean;
  /** GPU VRAM in MiB (default: engine default when GPU is enabled). Local target only. */
  gpuVramMib?: number;
  /**
   * Run the guest's unmodified CUDA/PyTorch code on the host's NVIDIA GPU by
   * remoting CUDA Driver-API calls over vsock (distinct from `gpu`, which is
   * Vulkan). On a host without an NVIDIA GPU this falls back to a CPU-emulation
   * backend. Local target only. Default: false.
   */
  cuda?: boolean;
  /** Network backend: `"tsi"` (outbound-only; the default for plain network
   *  access) or `"virtio-net"` (the default when a port, allow list or
   *  credential needs it). Choose `"virtio-net"` for a machine you will
   *  checkpoint and restore with a different network policy
   *  (`Machine.restoreCheckpoint(..., { networkPolicy })`): the policy is
   *  enforced by virtio-net's host-side stack, and a restore keeps the backend
   *  the checkpoint was taken with. Local target only. */
  networkBackend?: "tsi" | "virtio-net";
}

/** Host directory mounted into the machine. */
export interface MountSpec {
  /** Absolute path on the host. */
  source: string;
  /** Absolute path inside the machine. */
  target: string;
  /** Mount read-only. Default: false (writable), matching the `smol -v` CLI. */
  readOnly?: boolean;
  /** @deprecated Use `readOnly`. Kept for backwards compatibility. */
  readonly?: boolean;
  /**
   * Run from a guest-local working copy and copy changes back on `sync()`,
   * graceful stop, or delete. Local only; cannot be combined with `readOnly`.
   */
  staged?: boolean;
}

/** Host→guest port mapping. */
export interface PortSpec {
  host: number;
  guest: number;
}

/** Options for one live branch. Passing a `PortSpec[]` directly remains
 * supported for backwards compatibility. */
export interface BranchOptions {
  /** Optional pinned inbound port forwards. */
  ports?: PortSpec[];
  /** Materialize this child as a new branch source so it can be branched
   * again. This pays one eager guest-memory copy when the child boots. */
  branchable?: boolean;
  /** @deprecated Use `branchable`. */
  checkpointable?: boolean;
  /** Keep the source paused as a reusable branch base instead of resuming it.
   *  Later branches start from that same state, and a frozen source no longer
   *  counts toward the concurrency cap. The source cannot run commands again;
   *  work in its branches, or stop it. */
  freezeSource?: boolean;
}

/** Backwards-compatible name for {@link BranchOptions}. */
export type ForkOptions = BranchOptions;

/** Options for branching one source into many children at once. Provide either
 *  `count` (children auto-named `{namePrefix}-{n}`) or explicit `names`. */
export interface BranchBatchOptions {
  /** Number of children to branch (1..=64). Ignored when `names` is set. */
  count?: number;
  /** Explicit child names; its length is the batch size when set. */
  names?: string[];
  /** Prefix for auto-named children when `count` is used (default: the source
   *  name on the cloud target, else `"branch"`). */
  namePrefix?: string;
  /** Inbound port forwards applied to every child. Empty = each child gets fresh
   *  host ports so children don't collide. */
  ports?: PortSpec[];
  /** Keep the source paused as a reusable branch base instead of resuming it.
   *  Later branches start from that same state, and a frozen source no longer
   *  counts toward the concurrency cap. The source cannot run commands again;
   *  work in its branches, or stop it. */
  freezeSource?: boolean;
}

/** Backwards-compatible name for {@link BranchBatchOptions}. */
export type ForkBatchOptions = BranchBatchOptions;

/** Totals to grow a running machine to. Unset fields stay unchanged. RAM
 * and disks only grow; CPUs can also shrink on Linux x86_64. */
export interface ResizeOptions {
  /** Total vCPUs. */
  cpus?: number;
  /** Total memory in MB. */
  memoryMb?: number;
  /** Storage disk size in GB. */
  storageGb?: number;
  /** Overlay disk size in GB. */
  overlayGb?: number;
}

/** A machine's resources after a resize. */
export interface MachineResources {
  /** Online vCPUs. */
  cpus: number;
  /** Memory in MB. */
  memoryMb: number;
  /** Storage disk size in GB, or unset while it has the default size. */
  storageGb?: number;
  /** Overlay disk size in GB, or unset while it has the default size. */
  overlayGb?: number;
}

/** Options for a local checkpoint capture. A store reuses unchanged chunks
 * across periodic captures; cloud checkpoints are deduplicated server-side. */
export interface CheckpointOptions {
  /** Local content-addressed checkpoint store directory. */
  store?: string;
}

/** Options for assigning an RL episode — fork + provision one clone of a forkable
 *  golden under an idempotent lease. Cloud target only. */
export interface AssignOptions {
  /** Caller-chosen idempotency key; a retried assign returns the same episode. */
  leaseId: string;
  /** JSON-serializable task payload, staged into the clone at
   *  `/run/smol-task.json` before it's handed back. */
  task?: unknown;
  /** Files to stage into the clone: `{ guestPath: content }`. */
  files?: Record<string, Uint8Array>;
  /** Per-episode secrets, staged at `/run/smol-secrets.json`; never persisted. */
  secrets?: Record<string, string>;
  /** Optional pinned inbound port forwards. */
  ports?: PortSpec[];
  /** Lease time-to-live in seconds; the clone is reclaimed past it. */
  ttlSecs?: number;
  /** Expected heartbeat cadence in seconds; reclaimed if missed. */
  heartbeatSecs?: number;
}

/** Configuration for creating a machine. */
/**
 * A credential the workload uses without ever seeing it, like the CLI's
 * `--credential`. The guest variable `envVar` holds a placeholder; the real
 * value is substituted only in the headers of HTTPS requests to `hosts`, so
 * no other destination can receive it.
 */
export interface CredentialSpec {
  /** Binding name: 1-64 lowercase letters, digits, `-` and `_`. On the cloud
   *  it names the credential stored for your account. */
  name: string;
  /** Guest environment variable that holds the placeholder. */
  envVar: string;
  /** Exact host names the value may be sent to. No wildcards: list each
   *  subdomain. */
  hosts: string[];
  /**
   * The real value. Locally it is held in this process's memory only, never
   * written to disk; when omitted, this process's own `envVar` is read at
   * each start, which is also how a machine reopened by a later process gets
   * it. On the cloud it is stored sealed under `name` (replacing any stored
   * value); when omitted, the credential already stored under `name` is used.
   */
  value?: string;
  /** HTTP methods the value may be used with, e.g. `["GET", "HEAD"]` for a
   *  read-only token. Every method when omitted. (local) */
  methods?: string[];
}

export interface MachineConfig {
  /** Trusted host egress interceptor for local machines. Supply it again after reconnect. */
  egressInterceptor?: EgressInterceptor;
  /** Machine name (auto-generated if omitted). */
  name?: string;
  /** Base image. Required for the cloud target; optional for local (where you
   *  typically `run(image, …)` per command instead). */
  image?: string;
  /** Host directories to mount. (local) */
  mounts?: MountSpec[];
  /** Port mappings. On cloud, the guest port is published and the control plane
   *  allocates the host port; readiness includes that published port accepting
   *  connections. */
  ports?: PortSpec[];
  /** Locally, return from `Machine.create()` after the guest agent is ready,
   *  even if a published port is not accepting connections yet. Use this when
   *  `exec()` starts the service: the default port wait would deadlock before
   *  `exec()` can run. Call `machine.waitUntilReady()` after starting it.
   *  Default true; false is local-only and rejected on cloud. */
  waitForPorts?: boolean;
  /** Resource allocation. */
  resources?: ResourceSpec;
  /** Enable outbound network access. An alias for `resources.network`, which
   *  takes precedence when both are set. Accepted here because it is the
   *  shape callers reach for first, and was previously ignored without a
   *  word — a machine that asked for network quietly got none. For an
   *  allowlist rather than open access, set `resources.allowHosts` or
   *  `resources.allowCidrs`. */
  network?: boolean;
  /** Keep the machine record after the process exits (default: false). (local) */
  persistent?: boolean;
  /**
   * Let the machine outlive this process (default: false). A local machine is
   * normally tied to the process that created it: the engine reaps its VM
   * moments after that process dies, however it dies, and the SDK stops it on
   * SIGINT/SIGTERM. A detached machine keeps running through both — an exit,
   * a crash, a restart — and a later process picks it up again with
   * `Machine.connect(name)`, so give it a `name` you can find again (or record
   * `machine.name`). Implies `persistent`, and is remembered by the machine,
   * so later starts and branches are detached too. The machine then belongs
   * to whoever calls `stop()` or `delete()`; `Machine.list()` finds the ones
   * a process left behind. (local)
   */
  detach?: boolean;
  /**
   * Caller metadata stored with the machine and returned by `Machine.list()`,
   * e.g. `{ owner: "hostd", tenant: "acme" }`. Never interpreted by the
   * engine. This is how a process tells its own machines from everyone
   * else's after it restarts, since the shared machine database lists them
   * all. (local)
   */
  labels?: Record<string, string>;
  /** Auto-stop the machine after N idle seconds. (cloud) */
  autoStopSeconds?: number;
  /** Delete the machine after N seconds. (cloud) */
  ttlSeconds?: number;
  /** Start as a live-RAM branch source so the machine can produce independent
   *  copy-on-write children with `Machine.branch`. */
  branchable?: boolean;
  /** @deprecated Use `branchable`. */
  forkable?: boolean;
  /** @deprecated Use `branchable`; `checkpoint` now refers to a durable artifact. */
  checkpoint?: boolean;
  /** Environment variables for the image workload launched at create. */
  env?: Record<string, string>;
  /** Credentials the workload uses without seeing them. Implies network
   *  access. See {@link CredentialSpec}. */
  credentials?: CredentialSpec[];
  /** Start from a published version of one of your cache disks, mounted at
   *  its mount path (or `mountPath`) through the machine's own copy-on-write
   *  layer. Cloud only. See {@link CacheDisk}. */
  cacheDisk?: CacheDiskRef;
  /** Working directory for the image workload, set at create. Overrides
   *  the image's own workdir. */
  workdir?: string;
  /** Run the workload as this user: a name from the image or a numeric
   *  `uid[:gid]`. Overrides the image's USER, so a workload can match the
   *  owner of a mounted host directory. Local target only; the cloud target
   *  rejects it with `NotSupportedError`. */
  user?: string;
}

/** One machine as reported by `Machine.list()`. */
export interface MachineSummary {
  /** Machine name. */
  name: string;
  /** The cloud `mach-…` id, or the name on local. */
  id: string;
  /** Lifecycle state as `state()` would report it. */
  state: string;
  /** Base image, when the machine boots one. */
  image?: string;
  /** Caller metadata given at create. (local; empty on cloud) */
  labels: Record<string, string>;
  /** Host PID of the VM process while it is running. (local) */
  pid?: number;
  /** Whether the record outlives the process that created it. */
  persistent: boolean;
  /** Whether the VM outlives the process that starts it. Always true on
   *  cloud, where machines are remote. */
  detached: boolean;
  /** Whether it starts as a live branch source. */
  branchable: boolean;
  /** When the machine was created (RFC3339). */
  createdAt: string;
}

/** Filters for `Machine.list()`. */
export interface ListOptions {
  /** Only machines carrying every one of these labels with these values. */
  labels?: Record<string, string>;
}

/** An outbound network policy that can replace a stopped machine's own.
 *  `allowHosts` entries are exact host names (`api.github.com`) or subdomain
 *  wildcards (`*.github.com`, which does not match `github.com` itself). An
 *  allow list that names no hosts and no CIDRs allows nothing. */
export type NetworkPolicy =
  | "allow-all"
  | "deny-all"
  | { allowHosts?: string[]; allowCidrs?: string[] };

/** Options for `Machine.restoreCheckpoint`. */
export interface RestoreCheckpointOptions {
  /** Apply this network policy to the restored machine before it boots.
   *  Locally, the checkpoint must have been taken on the `"virtio-net"`
   *  backend for an allow list or `deny-all`. A cloud restore without one has
   *  networking blocked. */
  networkPolicy?: NetworkPolicy;
  /** Restore the machine as itself going back in time rather than as a clone:
   *  it keeps the hostname and machine ID it was saved with, which skips about
   *  a second of identity reset on its first start. For rewinding a machine to
   *  an earlier save point; do not run two machines from one checkpoint with
   *  it. Local target only. */
  keepIdentity?: boolean;
  /** Mount this cache in the slot the checkpoint's machine was created with
   *  (`cacheDisk: { cache, slot: true }`), in place of the cache it was
   *  captured with. It must be no larger than the slot and mount at the
   *  slot's path, so one base checkpoint resumes with each project's own
   *  cache. Cloud only. */
  cacheDisk?: CacheDiskRef;
}

/** Per-call execution options. */
export interface ExecOptions {
  /** Environment variables. */
  env?: Record<string, string>;
  /** Working directory inside the machine/container. */
  workdir?: string;
  /** Timeout in **seconds**. */
  timeout?: number;
  /** Run the command as this user: a name from the image or a numeric
   *  `uid[:gid]`. Locally this needs an image machine (a bare VM's agent runs
   *  every command as root, so it rejects `user`). On the cloud target the SDK
   *  first confirms the control plane honours per-command users and throws
   *  `NotSupportedError` — before running anything — if it predates them. It is
   *  never silently ignored. */
  user?: string;
  /** Abort the command. Locally the command is killed in the machine and the
   *  call (or the `execStream` iteration) rejects with `signal.reason`. On the
   *  cloud target the request is cancelled and the call rejects the same way;
   *  whether the command keeps running server-side is up to the service. Not
   *  supported by `run(image, argv)`. */
  signal?: AbortSignal;
  /** Cloud target only: which output encodings the server returns. The default
   *  carries both the capped UTF-8 text fields and the byte-exact base64
   *  fields; `"text"` or `"b64"` halves the response payload by dropping the
   *  other family. With `"text"`, `stdoutBytes`/`stderrBytes` degrade to the
   *  lossy text re-encoded; with `"b64"`, `stdout`/`stderr` are empty strings.
   *  Ignored (both families, as before) on older control planes and on the
   *  local target. */
  output?: "text" | "b64" | "both";
  /** Cloud only: start the command detached and return at once with its
   *  `pid`, leaving it running (a dev server, an agent) after the call ends. */
  background?: boolean;
}

/** Result of a command execution. */
export interface ExecResult {
  exitCode: number;
  /**
   * Captured stdout as text (UTF-8; invalid bytes are replaced), truncated to
   * ~1 MiB on the cloud target (see `stdoutTruncated`). This conversion is lossy
   * for binary output — use `stdoutBytes` for byte-exact, untruncated output, or
   * `execStream` to stream very large output.
   */
  stdout: string;
  stderr: string;
  /** True when the cloud capped the text `stdout` (1 MiB); `stdoutBytes` is still
   *  complete, or fetch big output via `execStream` / `readFile`. Always false on
   *  the local target (the embedded engine streams unbounded). */
  stdoutTruncated: boolean;
  /** True when the cloud capped the text `stderr` (1 MiB); see `stdoutTruncated`. */
  stderrTruncated: boolean;
  /** Byte-exact, untruncated stdout. Populated from the cloud's base64 output
   *  when the control provides it (older controls fall back to the UTF-8 bytes of
   *  the lossy `stdout`); on the local target it is the UTF-8 encoding of `stdout`.
   *  Prefer this over `stdout` for binary or >1 MiB output. */
  stdoutBytes: Uint8Array;
  /** The detached process's pid, for an exec run with `background: true`. */
  pid?: number;
  /** Byte-exact, untruncated stderr; see `stdoutBytes`. */
  stderrBytes: Uint8Array;
  /** True when exitCode === 0. */
  success: boolean;
  /** stdout + stderr, concatenated. */
  output: string;
  /** Throws ExecutionError if exitCode !== 0. */
  assertSuccess(): void;
}

/** A cached OCI image. */
export interface ImageInfo {
  reference: string;
  digest: string;
  /** Size in bytes. */
  size: number;
  architecture: string;
  os: string;
}

/** Event from a streaming execution. */
export type ExecEvent =
  | { kind: "stdout"; data: string }
  | { kind: "stderr"; data: string }
  | { kind: "exit"; exitCode: number }
  | { kind: "error"; message: string };

/** Options for `Machine.waitUntilReady`. */
export interface WaitReadyOptions {
  /** Give up after this many milliseconds (default: 120000). */
  timeoutMs?: number;
  /** Delay between readiness polls, in milliseconds (default: 1000). */
  intervalMs?: number;
}

/** A way to reach a PUBLISHED guest port. Local endpoints use the current
 *  localhost host-port mapping; cloud endpoints use the authenticated connect
 *  bridge. */
export interface PortEndpoint {
  /** `https://…/v1/machines/:id/connect/:port[/path]` — for HTTP requests. */
  httpUrl: string;
  /** `wss://…/v1/machines/:id/connect/:port[/path]` — for WebSocket upgrades. */
  wsUrl: string;
  /** Headers to send (the tenant Bearer token). */
  headers: Record<string, string>;
}

/** Metered usage totals for one machine over the report window. */
export interface MachineUsageTotals {
  totalUptimeSeconds: number;
  cpuHours: number;
  memoryGbHours: number;
  diskGbHours: number;
  egressGb: number;
}

/** Cost breakdown for one machine, in micro-dollars (1e-6 USD). */
export interface MachineCostBreakdown {
  cpuMicros: number;
  memoryMicros: number;
  diskMicros: number;
  egressMicros: number;
  baseMicros: number;
  totalMicros: number;
  amountDueMicros: number;
}

/** Per-machine usage + cost report (cloud target). Returned by
 *  `Machine.usage()` and `Machine.delete({ includeUsage: true })`; usage
 *  records survive deletion for 30 days. */
/** An anonymous share link for a machine's published app. Anyone holding the
 *  URL reaches the app without a smolmachines account, so treat it as a
 *  credential. Minting again replaces the previous token. */
export interface ShareLink {
  /** The share token. Attach as `?t=<token>` when `url` is null. */
  token: string;
  /** Ready-to-use URL, or null when the tenant has no apps domain configured
   *  or the machine name is not DNS-safe. */
  url: string | null;
}

export interface MachineUsageReport {
  machineId: string;
  /** Report window (RFC 3339). Starts at the current billing period. */
  from: string;
  to: string;
  usage: MachineUsageTotals;
  cost: MachineCostBreakdown;
}

/** Durable live machine checkpoint stored by the cloud control plane. */
export interface PortableCheckpointInfo {
  id: string;
  machineId: string;
  status: string;
  sizeBytes: number;
  /** Logical bytes reused from prior captures in the local store. */
  reusedBytes?: number;
  arch: string;
  createdAt: string;
  downloadUrl: string;
  /** Local artifact path when capture wrote directly to disk. */
  path?: string;
  /** Local source pause at the consistency boundary. */
  sourcePauseMs?: number;
  /** Local end-to-end capture and compression time. */
  elapsedMs?: number;
}

/** Selects and configures the backend. Local (embedded) is the default. */
export interface ConnectOptions {
  /** Locally, allow `Machine.connect()` to return after the guest agent is
   *  ready even when a published port is not serving. Useful when reconnecting
   *  to stop or delete a worker whose listener has exited. Default true;
   *  false is local-only and rejected on cloud. For `create`, put this setting
   *  on `MachineConfig` instead. */
  waitForPorts?: boolean;
  /** Binding for a stopped local machine that requires intercepted egress. */
  egressInterceptor?: EgressInterceptor;
  /** 'local' = embedded engine; 'cloud' = smolfleet remote. When unset, the
   *  SDK uses the cloud if `SMOL_CLOUD_TOKEN` is set, else local — so a
   *  library or framework embedding the SDK should set this explicitly. */
  target?: "local" | "cloud";
  /** Stop this process's local machines on SIGINT/SIGTERM, then re-raise the
   *  signal (default: true). Set false when the embedding app or framework
   *  owns shutdown: the re-raise would otherwise run its signal handlers a
   *  second time. It must then stop or delete its machines itself; if the
   *  process dies without doing so, the engine still reaps the VM. Local only;
   *  branches inherit their source's setting. */
  handleSignals?: boolean;
  /** Cloud base URL (cloud target only). */
  baseUrl?: string;
  /** Cloud API key, `smk_…` (cloud target only). */
  apiKey?: string;
  /** Cloud only: the `fetch` every cloud request goes through, for a proxy,
   *  retries, tracing, or a runtime without a global `fetch`. Its own errors
   *  reach the caller unchanged; timeouts and aborts are still reported as
   *  `TIMEOUT` and the abort reason. Default: the global `fetch`. */
  fetch?: typeof fetch;
  /** Cloud only: read a binary response body, for a `fetch` whose responses
   *  lack a working `arrayBuffer()`. */
  readResponseBytes?: (response: Response) => Promise<Uint8Array>;
}

/** Per-launch trusted host service. Keep the token out of logs and persistent config. */
export interface EgressInterceptor {
  /** Loopback socket address, for example `127.0.0.1:9000`. */
  address: string;
  /** 64 hexadecimal digits from the interceptor service. */
  token: string;
}

export interface StartOptions {
  egressInterceptor?: EgressInterceptor;
  /** Wait for the guest to be ready before returning (default true). Pass
   *  false to return once the start is accepted, e.g. when starting many
   *  machines and polling `ready()` yourself. */
  waitUntilReady?: boolean;
}

/** Options for `Machine.resume`. */
export interface ResumeOptions {
  /** Wait for the guest to be ready before returning (default true). */
  waitUntilReady?: boolean;
}

/** Which cache disk a machine starts from. */
export interface CacheDiskRef {
  /** The cache disk's id or name. */
  cache: string;
  /** Version to start from; the latest when omitted. */
  version?: number;
  /** Absolute guest path to mount it at; the cache disk's own when omitted. */
  mountPath?: string;
  /** On create, attach the cache unmounted as a slot: restoring a checkpoint
   *  of the machine with `restoreCheckpoint(..., { cacheDisk })` mounts any
   *  cache no larger than this one there, at this one's mount path. */
  slot?: boolean;
}

/** Options for creating a cache disk. */
export interface CreateCacheDiskOptions {
  /** Unique within your account: lowercase letters, digits, `.`, `_`, `-`. */
  name: string;
  /** Size of the cache filesystem in GiB (1-500). Default 20. */
  sizeGb?: number;
  /** Absolute guest path machines mount it at by default. Default `/cache`. */
  mountPath?: string;
}

/** One immutable version of a cache disk. */
export interface CacheDiskVersion {
  /** 0 is the empty filesystem a cache disk starts as; each publish adds one. */
  version: number;
  sizeBytes: number;
  sha256: string;
  createdAt: string;
  /** The machine this version was published from; absent for v0. */
  sourceMachineId?: string;
}

/** A cache disk: a disk image many machines start from, each through its own
 *  copy-on-write layer, published in immutable versions. */
export interface CacheDiskInfo {
  id: string;
  name: string;
  sizeGb: number;
  mountPath: string;
  latestVersion: number;
  /** Newest first. */
  versions: CacheDiskVersion[];
  createdAt: string;
}

/** What publishing a machine's cache disk created. */
export interface PublishedCacheDisk {
  cacheDisk: CacheDiskInfo;
  version: CacheDiskVersion;
}
