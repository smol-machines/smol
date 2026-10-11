/** Run Strands shell and file tools in a local or cloud Smol microVM.
 *
 * Install `smolmachines` and `@strands-agents/sdk`, then pass a `SmolSandbox`
 * to a Strands Agent. Call `close()` to delete an owned machine; a supplied
 * machine remains owned by the caller.
 */
import {
  PosixShellSandbox,
  SandboxAbortError,
  SandboxTimeoutError,
  type ExecuteOptions,
  type ExecutionResult,
  type StreamChunk,
} from "@strands-agents/sdk";
import { makeFileEditor, DEFAULT_FILE_EDITOR_DESCRIPTION } from "@strands-agents/sdk/vended-tools/file-editor";
import { makeShell, SANDBOX_SHELL_DESCRIPTION } from "@strands-agents/sdk/vended-tools/shell";
import type { Tool } from "@strands-agents/sdk";
import { Machine } from "./machine.js";
import type { ConnectOptions, MachineConfig } from "./types.js";

const ENV_KEY = /^[a-zA-Z_][a-zA-Z0-9_]*$/;

export interface SmolSandboxOptions {
  /** Configuration for a VM created on the first command. Defaults to a small Python image without egress. */
  config?: MachineConfig;
  /** Explicit backend; defaults to local even when SMOL_CLOUD_TOKEN is set. */
  conn?: ConnectOptions;
  /** Use an existing VM without taking ownership of its lifecycle. */
  machine?: Machine;
  /** Default directory in the guest, not on the host. */
  workingDir?: string;
}

/** A Strands sandbox that keeps one VM across shell, code, and file operations. */
export class SmolSandbox extends PosixShellSandbox {
  readonly workingDir: string;
  private readonly config: MachineConfig;
  private readonly conn: ConnectOptions;
  private readonly owned: boolean;
  private machinePromise: Promise<Machine> | undefined;
  private readonly active = new Set<AbortController>();
  private closed = false;
  private closePromise: Promise<void> | undefined;

  constructor(options: SmolSandboxOptions = {}) {
    super();
    this.workingDir = options.workingDir ?? "/workspace";
    if (!this.workingDir.startsWith("/")) {
      throw new Error("workingDir must be an absolute guest path");
    }
    this.conn = { target: "local", ...options.conn };
    this.config = options.config ?? {
      image: "python:3.12-alpine",
      resources: { cpus: 2, memoryMb: 2048, network: false },
      ...(this.conn.target === "cloud" ? { ttlSeconds: 3600 } : {}),
    };
    this.owned = options.machine === undefined;
    if (options.machine) this.machinePromise = Promise.resolve(options.machine);
  }

  /** VM id after it has been created, or undefined before first use. */
  get machineId(): Promise<string | undefined> {
    return this.machinePromise?.then((machine) => machine.id) ?? Promise.resolve(undefined);
  }

  private async getMachine(): Promise<Machine> {
    if (this.closed) throw new Error("Smol sandbox is closed");
    if (!this.machinePromise) {
      this.machinePromise = Machine.create(this.config, this.conn).catch((error: unknown) => {
        this.machinePromise = undefined;
        throw error;
      });
    }
    const machine = await this.machinePromise;
    if (this.closed) throw new SandboxAbortError();
    return machine;
  }

  override async *executeStreaming(
    command: string,
    options?: ExecuteOptions,
  ): AsyncGenerator<StreamChunk | ExecutionResult, void, undefined> {
    const timeout = options?.timeout;
    if (timeout !== undefined && (!Number.isFinite(timeout) || timeout <= 0 || timeout * 1000 > 2_147_483_647)) {
      throw new RangeError("timeout must be positive and less than 25 days");
    }
    if (options?.env) {
      for (const key of Object.keys(options.env)) {
        if (!ENV_KEY.test(key)) throw new Error(`Invalid environment variable name: ${key}`);
      }
    }
    if (options?.signal?.aborted) throw new SandboxAbortError();
    const machine = await this.getMachine();
    if (options?.signal?.aborted) throw new SandboxAbortError();

    const controller = new AbortController();
    const abort = () => controller.abort(new SandboxAbortError());
    const signal = options?.signal;
    signal?.addEventListener("abort", abort, { once: true });
    this.active.add(controller);
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const deadline = timeout === undefined ? undefined : performance.now() + timeout * 1000;
    const timer = timeout === undefined ? undefined : setTimeout(() => {
      timedOut = true;
      controller.abort(new SandboxTimeoutError(timeout, stdout, stderr));
    }, timeout * 1000);
    try {
      const execOptions = {
        workdir: options?.cwd ?? this.workingDir,
        // Both the native agent and cloud API accept whole seconds only.
        // Give the host timer the first chance to report a typed timeout,
        // while the guest retains its own deadline if the host disappears.
        ...(timeout !== undefined ? { timeout: Math.ceil(timeout) + 1 } : {}),
        ...(options?.env !== undefined ? { env: options.env } : {}),
        signal: controller.signal,
      };
      let exitCode: number | undefined;
      for await (const event of machine.execStream(["sh", "-c", command], execOptions)) {
        if (event.kind === "stdout" || event.kind === "stderr") {
          if (event.kind === "stdout") stdout += event.data;
          else stderr += event.data;
          yield { type: "streamChunk", data: event.data, streamType: event.kind };
        } else if (event.kind === "exit") {
          exitCode = event.exitCode;
          break;
        } else if (event.kind === "error") {
          throw new Error(event.message);
        }
      }
      if (timedOut || (deadline !== undefined && performance.now() >= deadline)) {
        throw new SandboxTimeoutError(timeout!, stdout, stderr);
      }
      if (signal?.aborted || this.closed) throw new SandboxAbortError();
      if (exitCode === undefined) throw new Error("Smol exec stream ended without an exit event");
      yield { type: "executionResult", exitCode, stdout, stderr, outputFiles: [] };
    } catch (error) {
      if (timedOut) throw new SandboxTimeoutError(timeout!, stdout, stderr);
      if (signal?.aborted || this.closed) throw new SandboxAbortError();
      throw error;
    } finally {
      controller.abort(new SandboxAbortError());
      signal?.removeEventListener("abort", abort);
      if (timer) clearTimeout(timer);
      this.active.delete(controller);
    }
  }

  override async readFile(path: string): Promise<Uint8Array> {
    return this.getMachine().then((machine) => machine.readFile(path));
  }

  override async writeFile(path: string, content: Uint8Array): Promise<void> {
    await (await this.getMachine()).writeFile(path, content);
  }

  override getTools(): Tool[] {
    return [
      makeFileEditor(this, {
        name: "sandbox_file_editor",
        description: `${DEFAULT_FILE_EDITOR_DESCRIPTION} Files are in a Smol microVM.`,
      }),
      makeShell(this, {
        name: "sandbox_shell",
        description: `${SANDBOX_SHELL_DESCRIPTION} Runs in a Smol microVM at ${this.workingDir}.`,
      }),
    ];
  }

  /** Cancel running commands and delete a VM created by this sandbox. Safe to retry on delete failure. */
  close(): Promise<void> {
    this.closed = true;
    for (const controller of this.active) controller.abort(new SandboxAbortError());
    if (this.closePromise) return this.closePromise;
    const closing = (async () => {
      if (this.machinePromise && this.owned) {
        await (await this.machinePromise).delete();
        this.machinePromise = undefined;
      }
    })();
    this.closePromise = closing.finally(() => { this.closePromise = undefined; });
    return this.closePromise;
  }
}
