/** Public types for the `smol` SDK. Backend-agnostic; mapped to the native
 *  addon (local) or, in a later phase, the cloud REST API. */

/** Lifecycle state of a machine. Cloud `"started"` means the VM process
 * launched, not that the guest agent or workload is ready; use `ready()` or
 * `waitUntilReady()` before doing work. */
export type MachineState = "created" | "started" | "running" | "stopped";

/** CPU / memory / disk / network allocation for a machine. */
export interface ResourceSpec {
  /** Number of vCPUs. */
  cpus?: number;
  /** Memory in MB. */
  memoryMb?: number;
  /** Enable outbound network access (TSI). Default: false. */
  network?: boolean;
  /**
   * Scope egress to these CIDR ranges. Setting this (or `allowHosts`) enables
   * networking and restricts it to the listed CIDRs. Cloud target only.
   */
  allowCidrs?: string[];
  /**
   * Scope egress to these hostnames and their subdomains (e.g.
   * `api.anthropic.com`). Setting this (or `allowCidrs`) enables networking and
   * restricts it to the listed hosts. Cloud target only.
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
}

/** Host→guest port mapping. */
export interface PortSpec {
  host: number;
  guest: number;
}

/** Options for forking a forkable golden into many clones at once — the RL
 *  fan-out primitive (GRPO group sampling / eval-task fan-out). Provide either
 *  `count` (clones auto-named `{namePrefix}-{n}`) or explicit `names`. */
export interface ForkBatchOptions {
  /** Number of clones to fork (1..=64). Ignored when `names` is set. */
  count?: number;
  /** Explicit clone names; its length is the batch size when set. */
  names?: string[];
  /** Prefix for auto-named clones when `count` is used (default: the golden's
   *  name on the cloud target, else `"fork"`). */
  namePrefix?: string;
  /** Inbound port forwards applied to every clone. Empty = each clone gets fresh
   *  host ports so clones don't collide. */
  ports?: PortSpec[];
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
export interface MachineConfig {
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
  /** Resource allocation. */
  resources?: ResourceSpec;
  /** Enable outbound network access. An alias for `resources.network`, which
   *  takes precedence when both are set. Accepted here because it is the
   *  shape callers reach for first, and was previously ignored without a
   *  word — a machine that asked for network quietly got none. */
  network?: boolean;
  /** Keep the machine record after the process exits (default: false). (local) */
  persistent?: boolean;
  /** Auto-stop the machine after N idle seconds. (cloud) */
  autoStopSeconds?: number;
  /** Delete the machine after N seconds. (cloud) */
  ttlSeconds?: number;
  /** Start as a live-RAM fork base (cloud) so the machine can be cloned with
   *  `Machine.fork`. The golden and its clones are pinned to one node. */
  forkable?: boolean;
  /** RL/agent-facing alias for `forkable`: mark this machine as a checkpoint
   *  you branch rollback-isolated clones from (`Machine.branch`). */
  checkpoint?: boolean;
  /** Environment variables for the image workload launched at create. */
  env?: Record<string, string>;
  /** Working directory for the image workload, set at create. Overrides
   *  the image's own workdir. */
  workdir?: string;
}

/** Per-call execution options. */
export interface ExecOptions {
  /** Environment variables. */
  env?: Record<string, string>;
  /** Working directory inside the machine/container. */
  workdir?: string;
  /** Timeout in **seconds**. */
  timeout?: number;
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

/** Selects and configures the backend. Local (embedded) is the default. */
export interface ConnectOptions {
  /** 'local' = embedded engine (default). 'cloud' = smolfleet remote. */
  target?: "local" | "cloud";
  /** Cloud base URL (cloud target only). */
  baseUrl?: string;
  /** Cloud API key, `smk_…` (cloud target only). */
  apiKey?: string;
}
