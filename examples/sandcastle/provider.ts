/** Sandcastle isolated provider backed by a local Smol microVM. */

import { createReadStream } from "node:fs";
import { mkdtemp, rm, stat, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, posix } from "node:path";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  createIsolatedSandboxProvider,
  type ExecResult,
  type IsolatedSandboxHandle,
  type IsolatedSandboxProvider,
} from "@ai-hero/sandcastle";
import type { Machine as SmolMachine } from "smolmachines";

const MAX_TAIL_CHARS = 64 * 1024;

const REPO_PATH = "/workspace/sandcastle";
const UPLOAD_CHUNK_BYTES = 4 * 1024 * 1024;

export interface SmolOptions {
  /** Registry image with git, tar, a shell, and the desired agent CLI installed. */
  readonly image: string;
  /** Allowed guest egress hosts, including the image registry and agent API. */
  readonly allowHosts: string[];
  /** CPU count (default: 2). */
  readonly cpus?: number;
  /** Guest RAM in MiB (default: 2048). */
  readonly memoryMb?: number;
  /** Environment variables added to the commands Sandcastle runs in the VM. */
  readonly env?: Record<string, string>;
  /** Maximum returned output per stream (default: 64 Ki characters). */
  readonly maxOutputTailChars?: number;
}

const tail = (previous: string, chunk: string, max: number) =>
  (previous + chunk).slice(-max);

const guestExecOk = async (machine: SmolMachine, argv: string[]) => {
  const result = await machine.exec(argv);
  if (result.exitCode !== 0) {
    throw new Error(
      `Smol guest command failed (${result.exitCode}): ${result.stderr}`,
    );
  }
};

const hostTar = (directory: string, archive: string): Promise<void> =>
  new Promise((resolve, reject) => {
    const child = spawn("tar", ["-cf", archive, "-C", directory, "."], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      stderr = (stderr + chunk).slice(-4096);
    });
    child.once("error", reject);
    child.once("close", (code) => {
      if (code === 0) resolve();
      else reject(new Error(`tar failed (${code}): ${stderr}`));
    });
  });

/** Create a separate local microVM for each Sandcastle isolated sandbox. */
export const smol = (options: SmolOptions): IsolatedSandboxProvider => {
  if (!options.image)
    throw new Error("Smol requires an image with git and the agent CLI");
  if (!options.allowHosts.length) {
    throw new Error(
      "Smol requires allowHosts for the image registry and agent API",
    );
  }
  const maxOutputTailChars = options.maxOutputTailChars ?? MAX_TAIL_CHARS;
  if (!Number.isSafeInteger(maxOutputTailChars) || maxOutputTailChars <= 0) {
    throw new Error("maxOutputTailChars must be a positive integer");
  }

  return createIsolatedSandboxProvider({
    name: "smol",
    env: options.env,
    create: async (createOptions): Promise<IsolatedSandboxHandle> => {
      const { Machine } = await import("smolmachines");
      const machine = await Machine.create({
        image: options.image,
        resources: {
          cpus: options.cpus ?? 2,
          memoryMb: options.memoryMb ?? 2048,
          allowHosts: options.allowHosts,
        },
      });
      try {
        await guestExecOk(machine, ["mkdir", "-p", REPO_PATH]);
      } catch (error) {
        await machine.delete().catch(() => {});
        throw error;
      }
      let closed = false;

      const uploadFile = async (hostPath: string, sandboxPath: string) => {
        const staged = `/tmp/sandcastle-upload-${randomUUID()}`;
        try {
          // Git bundles can be many GiB: keep both host and SDK buffers bounded.
          for await (const chunk of createReadStream(hostPath, {
            highWaterMark: UPLOAD_CHUNK_BYTES,
          })) {
            const part = `/tmp/sandcastle-upload-${randomUUID()}.part`;
            try {
              await machine.writeFile(part, chunk);
              await guestExecOk(machine, [
                "sh",
                "-c",
                'cat "$1" >> "$2"',
                "sh",
                part,
                staged,
              ]);
            } finally {
              await machine.exec(["rm", "-f", part]).catch(() => {});
            }
          }
          // Empty files have no chunks, so create the staged file explicitly.
          if ((await stat(hostPath)).size === 0) {
            await machine.writeFile(staged, Buffer.alloc(0));
          }
          await guestExecOk(machine, [
            "mkdir",
            "-p",
            posix.dirname(sandboxPath),
          ]);
          await guestExecOk(machine, ["mv", "-f", staged, sandboxPath]);
        } finally {
          await machine.exec(["rm", "-f", staged]).catch(() => {});
        }
      };

      return {
        worktreePath: REPO_PATH,
        exec: async (command, opts): Promise<ExecResult> => {
          const stdinPath =
            opts?.stdin === undefined
              ? undefined
              : `/tmp/sandcastle-stdin-${randomUUID()}`;
          const argv = stdinPath
            ? ["sh", "-c", 'exec sh -c "$1" < "$2"', "sh", command, stdinPath]
            : ["sh", "-c", command];
          let stdoutTail = "";
          let stderrTail = "";
          let pendingLine = "";
          let exitCode: number | undefined;
          try {
            if (stdinPath) {
              await machine.writeFile(stdinPath, Buffer.from(opts!.stdin!), 0o644);
            }
            for await (const event of machine.execStream(argv, {
              workdir: opts?.cwd ?? REPO_PATH,
              env: createOptions.env,
              ...(opts?.sudo ? { user: "root" } : {}),
            })) {
              if (event.kind === "stdout") {
                stdoutTail = tail(stdoutTail, event.data, maxOutputTailChars);
                if (opts?.onLine) {
                  pendingLine += event.data;
                  let newline: number;
                  while ((newline = pendingLine.indexOf("\n")) !== -1) {
                    opts.onLine(pendingLine.slice(0, newline));
                    pendingLine = pendingLine.slice(newline + 1);
                  }
                  // A process may stream without newlines; bound the partial line
                  // and report progress so the idle timer still sees activity.
                  if (pendingLine.length > maxOutputTailChars) {
                    opts.onLine(pendingLine);
                    pendingLine = "";
                  }
                }
              } else if (event.kind === "stderr") {
                stderrTail = tail(stderrTail, event.data, maxOutputTailChars);
              } else if (event.kind === "exit") {
                exitCode = event.exitCode;
              } else if (event.kind === "error") {
                throw new Error(event.message);
              }
            }
            if (pendingLine && opts?.onLine) opts.onLine(pendingLine);
            if (exitCode === undefined)
              throw new Error("Smol command ended without an exit status");
            return {
              stdout: stdoutTail,
              stderr: stderrTail,
              exitCode,
            };
          } finally {
            if (stdinPath)
              await machine.exec(["rm", "-f", stdinPath]).catch(() => {});
          }
        },
        copyIn: async (hostPath, sandboxPath): Promise<void> => {
          const info = await stat(hostPath);
          if (!info.isDirectory()) return uploadFile(hostPath, sandboxPath);

          const tempDir = await mkdtemp(join(tmpdir(), "sandcastle-smol-"));
          const archive = join(tempDir, "files.tar");
          const guestArchive = `/tmp/sandcastle-upload-${randomUUID()}.tar`;
          try {
            await hostTar(hostPath, archive);
            await uploadFile(archive, guestArchive);
            await guestExecOk(machine, ["mkdir", "-p", sandboxPath]);
            await guestExecOk(machine, [
              "tar",
              "-xf",
              guestArchive,
              "-C",
              sandboxPath,
            ]);
          } finally {
            await machine.exec(["rm", "-f", guestArchive]).catch(() => {});
            await rm(tempDir, { recursive: true, force: true });
          }
        },
        copyFileOut: async (sandboxPath, hostPath): Promise<void> => {
          const data = await machine.readFile(sandboxPath);
          await mkdir(dirname(hostPath), { recursive: true });
          await writeFile(hostPath, data);
        },
        close: async (): Promise<void> => {
          if (closed) return;
          await machine.delete();
          closed = true;
        },
      };
    },
  });
};
