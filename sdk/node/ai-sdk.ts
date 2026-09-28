/**
 * A smolmachines machine as a Vercel AI SDK sandbox session.
 *
 * ```ts
 * import { Machine } from "smolmachines";
 * import { createSandboxSession } from "smolmachines/ai-sdk";
 *
 * const machine = await Machine.create({ image: "node:22", network: true });
 * const result = await generateText({
 *   model, tools, prompt,
 *   experimental_sandbox: createSandboxSession(machine),
 * });
 * ```
 *
 * The returned object is structurally an AI SDK `Experimental_SandboxSession`
 * (and the I/O half of eve's `SandboxSession`); this module has no dependency
 * on the `ai` package. Relative paths resolve under the session root,
 * `/workspace` by default, which is also the default working directory.
 */

import type { Machine } from "./machine";
import type { ExecEvent } from "./types";

/** Options for {@link createSandboxSession}. */
export interface SandboxSessionOptions {
  /** Directory relative paths resolve from, and the default working directory.
   *  Created on first use. Default: `/workspace`. */
  root?: string;
  /** Run every command as this user (image machines only). Files written
   *  through the session, and the directories created for them, are owned by
   *  this user. */
  user?: string;
  /** Environment variables for every command; a command's own `env` wins. */
  env?: Record<string, string>;
  /** Replaces the default description given to the model. */
  description?: string;
}

/** Options for running a command (the AI SDK `SandboxProcessOptions`). */
export interface SandboxProcessOptions {
  command: string;
  workingDirectory?: string;
  env?: Record<string, string>;
  abortSignal?: AbortSignal;
}

interface ReadFileOptions {
  path: string;
  abortSignal?: AbortSignal;
}

interface WriteFileOptions<CONTENT> {
  path: string;
  content: CONTENT;
  abortSignal?: AbortSignal;
}

/** A process started with `spawn` (the AI SDK `SandboxProcess`). */
export interface SandboxProcess {
  readonly pid?: number;
  readonly stdout: ReadableStream<Uint8Array>;
  readonly stderr: ReadableStream<Uint8Array>;
  wait(): Promise<{ exitCode: number }>;
  kill(): Promise<void>;
}

/** The AI SDK `Experimental_SandboxSession` surface, plus the path helpers eve uses. */
export interface SmolSandboxSession {
  readonly description: string;
  readFile(options: ReadFileOptions): Promise<ReadableStream<Uint8Array> | null>;
  readBinaryFile(options: ReadFileOptions): Promise<Uint8Array | null>;
  readTextFile(
    options: ReadFileOptions & { encoding?: string; startLine?: number; endLine?: number },
  ): Promise<string | null>;
  writeFile(options: WriteFileOptions<ReadableStream<Uint8Array>>): Promise<void>;
  writeBinaryFile(options: WriteFileOptions<Uint8Array>): Promise<void>;
  writeTextFile(options: WriteFileOptions<string> & { encoding?: string }): Promise<void>;
  spawn(options: SandboxProcessOptions): Promise<SandboxProcess>;
  run(options: SandboxProcessOptions): Promise<{ exitCode: number; stdout: string; stderr: string }>;
  /** Anchor a path to the session root; absolute paths pass through. */
  resolvePath(path: string): string;
  /** Remove a file or directory. */
  removePath(options: { path: string; force?: boolean; recursive?: boolean; abortSignal?: AbortSignal }): Promise<void>;
}

/** Exit code reported for a process ended with `kill()`: 128 + SIGKILL. */
const KILLED_EXIT_CODE = 137;

const shellQuote = (value: string): string => `'${value.replace(/'/g, `'\\''`)}'`;

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) throw signal.reason ?? new DOMException("The operation was aborted.", "AbortError");
}

function isMissingFile(error: unknown): boolean {
  const e = error as { code?: string; message?: string } | undefined;
  return e?.code === "NOT_FOUND" || /no such file|not found|ENOENT|does not exist/i.test(e?.message ?? "");
}

async function collect(stream: ReadableStream<Uint8Array>, signal?: AbortSignal): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let size = 0;
  const reader = stream.getReader();
  try {
    for (;;) {
      throwIfAborted(signal);
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      size += value.byteLength;
    }
  } finally {
    reader.releaseLock();
  }
  const out = new Uint8Array(size);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.byteLength;
  }
  return out;
}

function decode(bytes: Uint8Array, encoding: string): string {
  const normalized = encoding.toLowerCase().replace(/_/g, "-");
  if (normalized === "utf-8" || normalized === "utf8") return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  return Buffer.from(bytes).toString(encoding as BufferEncoding);
}

function encode(text: string, encoding: string): Uint8Array {
  const normalized = encoding.toLowerCase().replace(/_/g, "-");
  if (normalized === "utf-8" || normalized === "utf8") return new TextEncoder().encode(text);
  return new Uint8Array(Buffer.from(text, encoding as BufferEncoding));
}

/** Lines `startLine`..`endLine` (1-based, inclusive); past EOF reads through EOF. */
function sliceLines(text: string, startLine?: number, endLine?: number): string {
  if (startLine === undefined && endLine === undefined) return text;
  const lines = text.split("\n");
  const start = Math.max(1, startLine ?? 1);
  const end = Math.min(lines.length, endLine ?? lines.length);
  if (start > end) return "";
  return lines.slice(start - 1, end).join("\n");
}

/** Wrap a smolmachines machine as an AI SDK sandbox session. */
export function createSandboxSession(machine: Machine, options: SandboxSessionOptions = {}): SmolSandboxSession {
  const root = (options.root ?? "/workspace").replace(/\/+$/, "") || "/";
  const user = options.user;
  const asUser = user !== undefined ? { user } : {};
  let rootReady: Promise<void> | undefined;

  const resolvePath = (path: string): string => {
    if (path.startsWith("/")) return path;
    const relative = path.replace(/^\.\/+/, "");
    return relative === "" || relative === "." ? root : `${root === "/" ? "" : root}/${relative}`;
  };

  // The root has to exist before it can be a working directory; create it
  // once, and try again on a later call if it failed.
  const ensureRoot = (): Promise<void> => {
    rootReady ??= machine
      .exec(["mkdir", "-p", root], asUser)
      .then((r) => {
        if (r.exitCode !== 0) throw new Error(`create ${root}: ${r.stderr.trim()}`);
      })
      .catch((e: unknown) => {
        rootReady = undefined;
        throw e;
      });
    return rootReady;
  };

  const execOptions = (opts: SandboxProcessOptions, signal: AbortSignal) => ({
    workdir: opts.workingDirectory ? resolvePath(opts.workingDirectory) : root,
    ...(options.env || opts.env ? { env: { ...options.env, ...opts.env } } : {}),
    ...asUser,
    signal,
  });

  const readBinaryFile = async ({ path, abortSignal }: ReadFileOptions): Promise<Uint8Array | null> => {
    throwIfAborted(abortSignal);
    try {
      return new Uint8Array(await machine.readFile(resolvePath(path)));
    } catch (e) {
      if (isMissingFile(e)) return null;
      throw e;
    }
  };

  const writeBinaryFile = async ({ path, content, abortSignal }: WriteFileOptions<Uint8Array>): Promise<void> => {
    throwIfAborted(abortSignal);
    const target = resolvePath(path);
    const parent = target.slice(0, target.lastIndexOf("/")) || "/";
    const made = await machine.exec(["mkdir", "-p", parent], { ...asUser, ...(abortSignal ? { signal: abortSignal } : {}) });
    if (made.exitCode !== 0) throw new Error(`create ${parent}: ${made.stderr.trim()}`);
    await machine.writeFile(target, Buffer.from(content));
    // File transfer writes as root; hand the file to the session's user.
    if (user !== undefined) {
      const owned = await machine.exec(["chown", user, target], { user: "root" });
      if (owned.exitCode !== 0) throw new Error(`chown ${target}: ${owned.stderr.trim()}`);
    }
  };

  const spawn = async (opts: SandboxProcessOptions): Promise<SandboxProcess> => {
    throwIfAborted(opts.abortSignal);
    await ensureRoot();
    const control = new AbortController();
    let killed = false;
    const onAbort = () => control.abort(opts.abortSignal?.reason);
    opts.abortSignal?.addEventListener("abort", onAbort, { once: true });

    let stdoutCtl!: ReadableStreamDefaultController<Uint8Array>;
    let stderrCtl!: ReadableStreamDefaultController<Uint8Array>;
    const stdout = new ReadableStream<Uint8Array>({ start: (c) => void (stdoutCtl = c) });
    const stderr = new ReadableStream<Uint8Array>({ start: (c) => void (stderrCtl = c) });
    const encoder = new TextEncoder();
    const closeStreams = (error?: unknown) => {
      for (const c of [stdoutCtl, stderrCtl]) {
        try {
          if (error === undefined) c.close();
          else c.error(error);
        } catch {
          // already closed
        }
      }
    };

    const events = machine.execStream(["sh", "-c", opts.command], execOptions(opts, control.signal));
    const done = (async (): Promise<{ exitCode: number }> => {
      let exitCode: number | undefined;
      try {
        for await (const event of events as AsyncGenerator<ExecEvent>) {
          if (event.kind === "stdout") stdoutCtl.enqueue(encoder.encode(event.data));
          else if (event.kind === "stderr") stderrCtl.enqueue(encoder.encode(event.data));
          else if (event.kind === "exit") exitCode = event.exitCode;
          else if (event.kind === "error") throw new Error(event.message);
        }
        closeStreams();
        return { exitCode: exitCode ?? -1 };
      } catch (e) {
        if (killed) {
          closeStreams();
          return { exitCode: KILLED_EXIT_CODE };
        }
        closeStreams(e);
        throw e;
      } finally {
        opts.abortSignal?.removeEventListener("abort", onAbort);
      }
    })();
    // Waiting is optional for callers; never leave an unhandled rejection.
    done.catch(() => {});

    return {
      stdout,
      stderr,
      wait: () => done,
      async kill() {
        if (killed) return;
        killed = true;
        control.abort(new DOMException("The process was killed.", "AbortError"));
        await done.catch(() => {});
      },
    };
  };

  const description =
    options.description ??
    [
      "A smolmachines microVM: a full Linux machine with its own kernel, isolated from the host.",
      `The working directory is ${root}; relative paths resolve there.`,
      "Commands run through sh -c, so shell syntax (pipes, redirects, &&) works.",
      "Installed tools and files persist for the life of the machine.",
    ].join(" ");

  return {
    description,
    resolvePath,
    readBinaryFile,
    async readFile(opts) {
      const bytes = await readBinaryFile(opts);
      if (bytes === null) return null;
      return new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(bytes);
          controller.close();
        },
      });
    },
    async readTextFile({ encoding = "utf-8", startLine, endLine, ...opts }) {
      const bytes = await readBinaryFile(opts);
      if (bytes === null) return null;
      return sliceLines(decode(bytes, encoding), startLine, endLine);
    },
    writeBinaryFile,
    async writeFile({ content, ...opts }) {
      await writeBinaryFile({ ...opts, content: await collect(content, opts.abortSignal) });
    },
    async writeTextFile({ content, encoding = "utf-8", ...opts }) {
      await writeBinaryFile({ ...opts, content: encode(content, encoding) });
    },
    spawn,
    async run(opts) {
      throwIfAborted(opts.abortSignal);
      await ensureRoot();
      const control = new AbortController();
      const onAbort = () => control.abort(opts.abortSignal?.reason);
      opts.abortSignal?.addEventListener("abort", onAbort, { once: true });
      try {
        const r = await machine.exec(["sh", "-c", opts.command], execOptions(opts, control.signal));
        return { exitCode: r.exitCode, stdout: r.stdout, stderr: r.stderr };
      } finally {
        opts.abortSignal?.removeEventListener("abort", onAbort);
      }
    },
    async removePath({ path, force, recursive, abortSignal }) {
      const flags = `${recursive ? "r" : ""}${force ? "f" : ""}`;
      const r = await machine.exec(["sh", "-c", `rm ${flags ? `-${flags} ` : ""}-- ${shellQuote(resolvePath(path))}`], {
        ...asUser,
        ...(abortSignal ? { signal: abortSignal } : {}),
      });
      if (r.exitCode !== 0) throw new Error(`remove ${resolvePath(path)}: ${r.stderr.trim()}`);
    },
  };
}
