import { createHash } from "node:crypto"
import path from "node:path"

/** What a project's sandbox is made of. Every field is optional in opencode.json. */
export interface SandboxOptions {
  /** OCI image the commands run in. Default: `node:22-bookworm` (git, curl, python3, a C toolchain, Node). */
  image?: string
  /** Shell inside the machine. Default: `bash`, falling back to `sh` when the image has no bash. */
  shell?: string
  /** Give the machine network access. Default: true. Ignored when an allow list is set. */
  network?: boolean
  /** Only these hosts are reachable (e.g. `registry.npmjs.org`). Setting this enables networking. */
  allowHosts?: string[]
  /** Only these CIDR ranges are reachable. Setting this enables networking. */
  allowCidrs?: string[]
  /** vCPUs. Default: the runtime's. */
  cpus?: number
  /** Memory in MiB. Default: the runtime's. */
  memoryMb?: number
  /** Environment variables set for every command. */
  env?: Record<string, string>
  /** Names of host environment variables to forward. Nothing is forwarded by default, so secrets in
   *  your shell stay out of the sandbox. */
  passEnv?: string[]
  /** Extra host directories to mount, as `{ source, target, readOnly }`. The project is always mounted. */
  mounts?: { source: string; target: string; readOnly?: boolean }[]
  /** What to do with the machine when OpenCode exits: `stop` (default; installed packages survive for
   *  the next session), `delete`, or `keep` (leave it running). */
  onExit?: "stop" | "delete" | "keep"
}

/** The subset of a smolmachines `Machine` the sandbox drives. */
export interface MachineHandle {
  readonly name: string
  exec(
    command: string[],
    opts?: { env?: Record<string, string>; workdir?: string; timeout?: number; signal?: AbortSignal },
  ): Promise<{ exitCode: number; stdout: string; stderr: string; stdoutTruncated?: boolean; stderrTruncated?: boolean }>
  state(): Promise<string>
  start(): Promise<void>
  stop(): Promise<void>
  delete(): Promise<void>
}

/** How the sandbox reaches machines; the smolmachines SDK in production, a fake in tests. */
export interface MachineApi {
  create(config: Record<string, unknown>): Promise<MachineHandle>
  connect(name: string): Promise<MachineHandle>
}

export interface CommandResult {
  title: string
  output: string
  metadata: Record<string, unknown>
}

export const DEFAULT_IMAGE = "node:22-bookworm"
export const DEFAULT_TIMEOUT_MS = 2 * 60 * 1000
/** Model context is the scarce resource; keep the start and the end of long output. */
export const MAX_OUTPUT_CHARS = 30_000
const CONFIG_LABEL = "opencode-smolmachines-config"
/** smolvm reports an exec that outlived its timeout as exit 124. */
const TIMEOUT_EXIT = 124
/** 128 + SIGKILL: a command killed with the container it ran in. */
const KILLED_EXIT = 137
/** Probes before giving up on waiting; a workload restarts at most once per boot. */
const SETTLE_ATTEMPTS = 5

function slug(text: string): string {
  return (
    text
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 24) || "project"
  )
}

/** One machine per project, stable across sessions: `opencode-<dir>-<hash of its path>`. */
export function machineName(worktree: string): string {
  const hash = createHash("sha256").update(path.resolve(worktree)).digest("hex").slice(0, 8)
  return `opencode-${slug(path.basename(worktree))}-${hash}`
}

/** The machine's shape. A reused machine whose shape no longer matches the options is recreated. */
export function machineConfig(worktree: string, options: SandboxOptions): Record<string, unknown> {
  const restricted = Boolean(options.allowHosts?.length || options.allowCidrs?.length)
  const resources: Record<string, unknown> = {}
  if (options.cpus) resources.cpus = options.cpus
  if (options.memoryMb) resources.memoryMb = options.memoryMb
  if (options.allowHosts?.length) resources.allowHosts = options.allowHosts
  if (options.allowCidrs?.length) resources.allowCidrs = options.allowCidrs
  const root = path.resolve(worktree)
  return {
    name: machineName(root),
    image: options.image ?? DEFAULT_IMAGE,
    // The project keeps its host path inside the machine, so absolute paths in commands and
    // tool output mean the same thing on both sides.
    mounts: [{ source: root, target: root }, ...(options.mounts ?? [])],
    network: restricted ? true : (options.network ?? true),
    resources,
    // A local machine is otherwise reaped when the OpenCode process exits; detached, it survives
    // the session so the next one reuses it (and everything installed in it).
    detach: true,
  }
}

export function configFingerprint(config: Record<string, unknown>): string {
  return createHash("sha256").update(JSON.stringify(config)).digest("hex").slice(0, 16)
}

/** Keep the head and the tail; the middle of a long log is rarely what the model needs. */
export function truncate(text: string, max = MAX_OUTPUT_CHARS): string {
  if (text.length <= max) return text
  const half = Math.floor(max / 2)
  const dropped = text.length - 2 * half
  return `${text.slice(0, half)}\n\n... ${dropped} characters truncated ...\n\n${text.slice(-half)}`
}

/** The sandboxed shell for one OpenCode project. */
export class Sandbox {
  private machine?: Promise<MachineHandle>
  private shell?: Promise<string>
  private readonly config: Record<string, unknown>
  private readonly root: string

  constructor(
    worktree: string,
    private readonly options: SandboxOptions,
    private readonly api: MachineApi,
    private readonly hostEnv: Record<string, string | undefined> = process.env,
  ) {
    this.root = path.resolve(worktree)
    this.config = machineConfig(this.root, options)
  }

  get name(): string {
    return this.config.name as string
  }

  /** Reuse the project's machine when its shape still matches, else build a fresh one. */
  machineHandle(): Promise<MachineHandle> {
    if (!this.machine) {
      this.machine = this.acquire().catch((error) => {
        this.machine = undefined
        throw error
      })
    }
    return this.machine
  }

  private async acquire(): Promise<MachineHandle> {
    const fingerprint = configFingerprint(this.config)
    const labels = { "opencode-smolmachines": "true", [CONFIG_LABEL]: fingerprint }
    let existing: MachineHandle | undefined
    try {
      existing = await this.api.connect(this.name)
    } catch (error) {
      if ((error as { code?: string }).code !== "NOT_FOUND") throw error
    }
    if (existing) {
      if ((await existing.state()) !== "running") await existing.start()
      await this.settle(existing)
      const recorded = await this.recordedFingerprint(existing)
      if (recorded === fingerprint) return existing
      // Options changed (image, mounts, network policy): a stale machine would quietly run
      // commands with the old policy, so replace it.
      await existing.delete()
    }
    const machine = await this.api.create({ ...this.config, labels })
    await this.settle(machine)
    await this.writeFingerprint(machine, fingerprint)
    return machine
  }

  /**
   * Wait out the image's own workload. smolvm runs commands inside the container the image's
   * default command started, so when that command exits on its own (a bare `node` or `python`
   * with no input, as many images do) it takes any command still running with it, as exit 137.
   * After that the machine's commands run in a container that stays up. Boot (and every restart)
   * re-runs the workload, so a short probe has to survive before real commands go in.
   */
  private async settle(machine: MachineHandle): Promise<void> {
    for (let attempt = 0; attempt < SETTLE_ATTEMPTS; attempt++) {
      const probe = await machine.exec(["sh", "-c", "sleep 0.5"], { timeout: 10 })
      if (probe.exitCode !== KILLED_EXIT) return
    }
  }

  /** The fingerprint lives in the machine itself, so a reused machine can prove its shape. */
  private async recordedFingerprint(machine: MachineHandle): Promise<string | undefined> {
    try {
      const result = await machine.exec(["cat", "/etc/opencode-smolmachines"], { timeout: 10 })
      return result.exitCode === 0 ? result.stdout.trim() : undefined
    } catch {
      return undefined
    }
  }

  private async writeFingerprint(machine: MachineHandle, fingerprint: string): Promise<void> {
    const result = await machine.exec(
      ["sh", "-c", `printf %s "$1" > /etc/opencode-smolmachines`, "sh", fingerprint],
      { timeout: 10 },
    )
    // Unrecorded, the next session would take this machine for a stale one and rebuild it,
    // throwing away everything installed in it.
    if (result.exitCode !== 0) {
      throw new Error(`could not record the sandbox configuration: ${result.stderr.trim() || `exit ${result.exitCode}`}`)
    }
  }

  private shellPath(machine: MachineHandle): Promise<string> {
    if (!this.shell) {
      const wanted = this.options.shell ?? "bash"
      this.shell = machine
        .exec(["sh", "-c", `command -v "$1" || command -v sh`, "sh", wanted], { timeout: 10 })
        .then((result) => result.stdout.trim().split("\n")[0] || "sh")
    }
    return this.shell
  }

  /** Commands only see what is mounted; refuse a working directory the machine cannot reach. */
  resolveWorkdir(directory: string, workdir?: string): string {
    const target = path.resolve(directory, workdir ?? ".")
    const roots = [this.root, ...(this.options.mounts ?? []).map((m) => path.resolve(m.target))]
    const inside = roots.some((root) => target === root || target.startsWith(root + path.sep))
    if (!inside) {
      throw new Error(
        `${target} is outside the sandbox. Commands run in a smol machine that can only see ${roots.join(", ")}.`,
      )
    }
    return target
  }

  private commandEnv(): Record<string, string> {
    const env: Record<string, string> = {}
    for (const name of this.options.passEnv ?? []) {
      const value = this.hostEnv[name]
      if (value !== undefined) env[name] = value
    }
    return { ...env, ...(this.options.env ?? {}) }
  }

  async run(input: {
    command: string
    directory: string
    workdir?: string
    timeoutMs?: number
    signal?: AbortSignal
    onStart?: () => void
  }): Promise<CommandResult> {
    const workdir = this.resolveWorkdir(input.directory, input.workdir)
    const timeoutMs = input.timeoutMs ?? DEFAULT_TIMEOUT_MS
    const fresh = !this.machine
    if (fresh) input.onStart?.()
    const machine = await this.machineHandle()
    const shell = await this.shellPath(machine)
    let result
    try {
      result = await machine.exec([shell, "-c", input.command], {
        env: this.commandEnv(),
        workdir,
        timeout: Math.max(1, Math.ceil(timeoutMs / 1000)),
        signal: input.signal,
      })
    } catch (error) {
      if (input.signal?.aborted) {
        return {
          title: input.command,
          output: "Command aborted.",
          metadata: { exit: null, aborted: true, sandbox: "smolmachines", machine: this.name },
        }
      }
      throw error
    }
    const stderr = result.stderr.replace(/\n?command timed out after \d+ms\s*$/, "")
    const timedOut = result.exitCode === TIMEOUT_EXIT && stderr !== result.stderr
    let output = [result.stdout, stderr].filter((part) => part.length > 0).join(result.stdout.endsWith("\n") ? "" : "\n")
    output = output.replace(/\n+$/, "")
    if (timedOut) {
      output += `\n\nCommand terminated after exceeding its ${timeoutMs} ms timeout. If it is expected to run longer, retry with a larger timeout.`
    } else if (result.exitCode !== 0) {
      output += `\n\nExit code: ${result.exitCode}`
    }
    return {
      title: input.command,
      output: truncate(output.length > 0 ? output : "(no output)"),
      metadata: {
        exit: result.exitCode,
        timedOut,
        sandbox: "smolmachines",
        machine: this.name,
      },
    }
  }

  /** Called when OpenCode shuts the plugin down. */
  async dispose(): Promise<void> {
    if (!this.machine) return
    const onExit = this.options.onExit ?? "stop"
    if (onExit === "keep") return
    const machine = await this.machine.catch(() => undefined)
    if (!machine) return
    try {
      if (onExit === "delete") await machine.delete()
      else await machine.stop()
    } catch {
      // Shutting down; a machine left running is recovered or reused next session.
    }
  }
}
