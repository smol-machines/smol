/** Public types for the `smol` SDK. Backend-agnostic; mapped to the native
 *  addon (local) or, in a later phase, the cloud REST API. */

/** Lifecycle state of a machine. */
export type MachineState = "created" | "running" | "stopped";

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
}

/** Host directory mounted into the machine. */
export interface MountSpec {
  /** Absolute path on the host. */
  source: string;
  /** Absolute path inside the machine. */
  target: string;
  /** Mount read-only (default: true). */
  readonly?: boolean;
}

/** Host→guest port mapping. */
export interface PortSpec {
  host: number;
  guest: number;
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
  /** Port mappings. (local) */
  ports?: PortSpec[];
  /** Resource allocation. */
  resources?: ResourceSpec;
  /** Keep the machine record after the process exits (default: false). (local) */
  persistent?: boolean;
  /** Auto-stop the machine after N idle seconds. (cloud) */
  autoStopSeconds?: number;
  /** Delete the machine after N seconds. (cloud) */
  ttlSeconds?: number;
  /** Start as a live-RAM fork base (cloud) so the machine can be cloned with
   *  `Machine.fork`. The golden and its clones are pinned to one node. */
  forkable?: boolean;
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
   * Captured stdout as text (UTF-8; invalid bytes are replaced). For BINARY
   * output, read it back with `readFile()` instead — the string conversion is
   * lossy. Very large output (>~20 MB) is rejected; use `execStream` for that.
   */
  stdout: string;
  stderr: string;
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

/** Selects and configures the backend. Local (embedded) is the default. */
export interface ConnectOptions {
  /** 'local' = embedded engine (default). 'cloud' = remote (not yet implemented). */
  target?: "local" | "cloud";
  /** Cloud base URL (cloud target only). */
  baseUrl?: string;
  /** Cloud API key, `smk_…` (cloud target only). */
  apiKey?: string;
}
