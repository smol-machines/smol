#!/usr/bin/env node
/**
 * smolmachines-mcp — an MCP server that gives an agent its own branchable
 * microVM.
 *
 * The agent, not the human, assembles the environment: it creates a machine of
 * an image with the project copied in, runs commands there, branches before a
 * risky step and keeps or discards the result, and saves points it can come
 * back to. Everything runs on the local engine bundled with the `smolmachines`
 * SDK; set `SMOL_CLOUD_TOKEN` (or `SMOL_TARGET=cloud`) and the same tools run
 * on smol cloud.
 *
 * Start it from any MCP client:
 *   npx -y smolmachines-mcp
 */

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { Machine } from "smolmachines";
import type { ConnectOptions, MachineConfig } from "smolmachines";
import { spawn } from "node:child_process";
import { mkdir, stat } from "node:fs/promises";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import path from "node:path";

const { version: VERSION } = createRequire(import.meta.url)("../package.json") as { version: string };

/** Text returned per stream before it is cut; the full output stays in the machine. */
const OUTPUT_CAP = 64 * 1024;
/** Largest workspace or export we will move through the agent channel. */
const TRANSFER_CAP = 512 * 1024 * 1024;
/** Cloud lifetime of a machine created with `persistent: false`, should this server die before deleting it. */
const EPHEMERAL_TTL_SECONDS = 3600;

const target: "local" | "cloud" =
  process.env.SMOL_TARGET === "cloud" || process.env.SMOL_TARGET === "local"
    ? process.env.SMOL_TARGET
    : process.env.SMOL_CLOUD_TOKEN
      ? "cloud"
      : "local";
// The MCP client owns this process's lifetime; it must not re-raise signals.
const conn: ConnectOptions = { target, handleSignals: false };

const machines = new Map<string, Machine>();
let branchCounter = 0;
/** Machines created with `persistent: false`, and their branches, in creation order. */
const ephemeral = new Set<string>();

async function machine(name: string): Promise<Machine> {
  const known = machines.get(name);
  if (known) return known;
  const connected = await Machine.connect(name, conn);
  machines.set(name, connected);
  return connected;
}

function text(value: unknown) {
  return {
    content: [
      {
        type: "text" as const,
        text: typeof value === "string" ? value : JSON.stringify(value, null, 2),
      },
    ],
  };
}

function failure(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return { isError: true as const, content: [{ type: "text" as const, text: message }] };
}

function cap(output: string): { text: string; truncated: boolean } {
  if (output.length <= OUTPUT_CAP) return { text: output, truncated: false };
  return {
    text: `${output.slice(0, OUTPUT_CAP)}\n…[${output.length - OUTPUT_CAP} more characters; rerun with a narrower command or read a file]`,
    truncated: true,
  };
}

/** Run a host command, feeding `input` to stdin and collecting stdout. */
function host(
  command: string,
  args: string[],
  input?: Buffer,
): Promise<{ stdout: Buffer; stderr: string; code: number }> {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ["pipe", "pipe", "pipe"] });
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    let size = 0;
    child.stdout.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > TRANSFER_CAP) {
        child.kill();
        reject(new Error(`output exceeds ${TRANSFER_CAP} bytes`));
        return;
      }
      out.push(chunk);
    });
    child.stderr.on("data", (chunk: Buffer) => err.push(chunk));
    child.on("error", reject);
    child.on("close", (code) =>
      resolve({
        stdout: Buffer.concat(out),
        stderr: Buffer.concat(err).toString("utf8"),
        code: code ?? -1,
      }),
    );
    if (input) child.stdin.end(input);
    else child.stdin.end();
  });
}

const sh = (script: string) => ["sh", "-lc", script];
/** Single-quote a value for the guest shell so agent-supplied paths cannot break out. */
const q = (value: string) => `'${value.replace(/'/g, `'\\''`)}'`;
const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** Copy a host directory into the machine as a tarball the guest unpacks. */
async function copyIn(m: Machine, hostDir: string, guestDir: string, exclude: string[]) {
  const info = await stat(hostDir);
  if (!info.isDirectory()) throw new Error(`${hostDir} is not a directory`);
  const args = ["-C", hostDir, ...exclude.flatMap((pattern) => ["--exclude", pattern]), "-czf", "-", "."];
  const tar = await host("tar", args);
  if (tar.code !== 0) throw new Error(`tar on host failed: ${tar.stderr}`);
  await m.writeFile("/tmp/.smol-workspace.tgz", tar.stdout);
  const unpack = await m.exec(
    sh(`mkdir -p ${q(guestDir)} && tar -xzf /tmp/.smol-workspace.tgz -C ${q(guestDir)} && rm -f /tmp/.smol-workspace.tgz`),
    { timeout: 600 },
  );
  if (unpack.exitCode !== 0) throw new Error(`unpack in machine failed: ${unpack.stderr}`);
  return tar.stdout.length;
}

const server = new McpServer(
  { name: "smolmachines", version: VERSION },
  {
    instructions: [
      "Each machine is a real Linux microVM with its own kernel; the host is not reachable except through these tools.",
      "Typical flow: smol_create with the project as `workspace` (it is copied to /workspace), then smol_exec to work.",
      "Before a risky change, call smol_branch and continue in the child. If it worked, keep working there or smol_export the results; if not, smol_delete the child and the source is untouched.",
      "smol_checkpoint saves a point you can smol_restore into a fresh machine later. Branching and checkpoints cost only what changed.",
      `Tools run on the ${target} target.`,
    ].join(" "),
  },
);

server.registerTool(
  "smol_create",
  {
    title: "Create a machine",
    description:
      "Boot an isolated Linux microVM from a container image and, optionally, copy a host directory into it as /workspace. Returns the machine name used by every other tool. Network is on by default; pass allowHosts to restrict egress to named hosts.",
    inputSchema: {
      name: z.string().optional().describe("Machine name (generated when omitted)."),
      image: z
        .string()
        .default("alpine:3.20")
        .describe("OCI image to boot, e.g. python:3.12, node:22, ubuntu:24.04."),
      workspace: z
        .string()
        .optional()
        .describe("Absolute host directory to copy into the machine at `workspaceTarget`."),
      workspaceTarget: z.string().default("/workspace"),
      exclude: z
        .array(z.string())
        .default([])
        .describe("tar exclude patterns applied to the workspace copy, e.g. node_modules."),
      cpus: z.number().int().min(1).max(64).default(2),
      memoryMb: z.number().int().min(256).default(2048),
      network: z.boolean().default(true),
      allowHosts: z
        .array(z.string())
        .optional()
        .describe("Restrict egress to these hosts (e.g. api.anthropic.com, registry.npmjs.org)."),
      env: z.record(z.string()).optional().describe("Environment for every command run in the machine."),
      persistent: z
        .boolean()
        .default(true)
        .describe("Keep the machine record after this server exits so a later session can reconnect by name."),
    },
  },
  async (args) => {
    try {
      const config: MachineConfig = {
        name: args.name,
        image: args.image,
        branchable: true,
        persistent: args.persistent,
        // Cloud only; locally the engine reaps a dead server's VMs itself.
        ...(args.persistent ? {} : { ttlSeconds: EPHEMERAL_TTL_SECONDS }),
        resources: {
          cpus: args.cpus,
          memoryMb: args.memoryMb,
          network: args.network,
          ...(args.allowHosts ? { allowHosts: args.allowHosts } : {}),
        },
      };
      const started = Date.now();
      const m = await Machine.create(config, conn);
      machines.set(m.name, m);
      if (!args.persistent) ephemeral.add(m.name);
      let copied: number | undefined;
      if (args.workspace) copied = await copyIn(m, args.workspace, args.workspaceTarget, args.exclude);
      if (args.env) {
        const bad = Object.keys(args.env).find((key) => !ENV_KEY.test(key));
        if (bad) throw new Error(`env key is not a POSIX variable name: ${bad}`);
        const lines = Object.entries(args.env)
          .map(([key, value]) => `export ${key}=${q(value)}`)
          .join("\n");
        await m.writeFile("/etc/profile.d/smol-mcp.sh", `${lines}\n`, 0o644);
      }
      return text({
        machine: m.name,
        image: args.image,
        target,
        workspace: args.workspace ? { from: args.workspace, to: args.workspaceTarget, bytes: copied } : undefined,
        ms: Date.now() - started,
      });
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_exec",
  {
    title: "Run a command",
    description:
      "Run a shell command inside a machine (via `sh -lc`) and return its exit code, stdout and stderr. Long outputs are cut at 64 KiB per stream.",
    inputSchema: {
      machine: z.string(),
      command: z.string().describe("Shell command line."),
      workdir: z.string().optional().describe("Working directory inside the machine (default: /workspace if it exists)."),
      timeoutSec: z.number().int().min(1).max(86400).default(600),
      env: z.record(z.string()).optional(),
    },
  },
  async (args) => {
    try {
      const m = await machine(args.machine);
      const workdir =
        args.workdir ??
        ((await m.exec(sh("test -d /workspace"))).exitCode === 0 ? "/workspace" : undefined);
      const result = await m.exec(sh(args.command), {
        workdir,
        timeout: args.timeoutSec,
        env: args.env,
      });
      const out = cap(result.stdout);
      const err = cap(result.stderr);
      return {
        isError: result.exitCode !== 0 ? true : undefined,
        content: [
          {
            type: "text" as const,
            text: JSON.stringify(
              {
                exitCode: result.exitCode,
                stdout: out.text,
                stderr: err.text,
                truncated: out.truncated || err.truncated || undefined,
              },
              null,
              2,
            ),
          },
        ],
      };
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_branch",
  {
    title: "Branch a machine",
    description:
      "Fork a running machine into an independent child that continues from the exact same state: same files, same processes, same memory. Takes about a tenth of a second and costs only what the child later changes. Use it before a risky or exploratory step, then keep working in the child or smol_delete it. The child is itself branchable.",
    inputSchema: {
      source: z.string(),
      name: z.string().optional().describe("Child name (default: `<source>-b<n>`)."),
    },
  },
  async (args) => {
    try {
      const source = await machine(args.source);
      const name = args.name ?? `${args.source}-b${++branchCounter}-${Date.now().toString(36)}`;
      const started = Date.now();
      const child = await source.branch(name, { branchable: true });
      machines.set(child.name, child);
      if (ephemeral.has(args.source)) ephemeral.add(child.name);
      return text({ machine: child.name, source: args.source, ms: Date.now() - started });
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_checkpoint",
  {
    title: "Save a checkpoint",
    description:
      "Save a machine's complete state (RAM, processes, disks) to a checkpoint you can later smol_restore into a new machine. The machine keeps running.",
    inputSchema: {
      machine: z.string(),
      output: z
        .string()
        .optional()
        .describe("Checkpoint path on the host (default: ~/.smol/checkpoints/<machine>-<time>.smolcheckpoint). Ignored on the cloud target."),
    },
  },
  async (args) => {
    try {
      const m = await machine(args.machine);
      let output = args.output;
      if (target === "local" && !output) {
        const dir = path.join(homedir(), ".smol", "checkpoints");
        await mkdir(dir, { recursive: true });
        output = path.join(dir, `${args.machine}-${Date.now().toString(36)}.smolcheckpoint`);
      }
      const started = Date.now();
      const info = await m.checkpoint(output);
      return text({ machine: args.machine, checkpoint: output ?? info.id, id: info.id, sizeBytes: info.sizeBytes, ms: Date.now() - started });
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_restore",
  {
    title: "Restore a checkpoint",
    description:
      "Start a new machine from a checkpoint saved with smol_checkpoint. This is how you rewind: the restored machine is the saved state, processes included, and is branchable.",
    inputSchema: {
      checkpoint: z.string().describe("Checkpoint path (local) or checkpoint id (cloud)."),
      name: z.string().optional(),
    },
  },
  async (args) => {
    try {
      const name = args.name ?? `restored-${Date.now().toString(36)}`;
      const started = Date.now();
      const m = await Machine.restoreCheckpoint(args.checkpoint, name, conn);
      machines.set(m.name, m);
      return text({ machine: m.name, checkpoint: args.checkpoint, ms: Date.now() - started });
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_read_file",
  {
    title: "Read a file",
    description: "Read a text file from a machine (cut at 256 KiB).",
    inputSchema: { machine: z.string(), path: z.string() },
  },
  async (args) => {
    try {
      const m = await machine(args.machine);
      const data = await m.readFile(args.path);
      const limit = 256 * 1024;
      const body = data.subarray(0, limit).toString("utf8");
      return text(data.length > limit ? `${body}\n…[${data.length - limit} more bytes]` : body);
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_write_file",
  {
    title: "Write a file",
    description: "Write a text file into a machine, creating parent directories.",
    inputSchema: {
      machine: z.string(),
      path: z.string(),
      content: z.string(),
      mode: z.number().int().optional().describe("Unix mode, e.g. 493 for 0755."),
    },
  },
  async (args) => {
    try {
      const m = await machine(args.machine);
      await m.exec(sh(`mkdir -p ${q(path.posix.dirname(args.path))}`));
      await m.writeFile(args.path, args.content, args.mode);
      return text({ machine: args.machine, path: args.path, bytes: Buffer.byteLength(args.content) });
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_export",
  {
    title: "Copy out of a machine",
    description:
      "Copy a directory (or file) from a machine to a host directory. Use it to bring a branch's results back after reviewing them; nothing leaves a machine unless you call this.",
    inputSchema: {
      machine: z.string(),
      guestPath: z.string().describe("Directory or file inside the machine."),
      hostPath: z.string().describe("Absolute host directory to extract into (created if missing)."),
    },
  },
  async (args) => {
    try {
      const m = await machine(args.machine);
      await mkdir(args.hostPath, { recursive: true });
      const parent = path.posix.dirname(args.guestPath);
      const base = path.posix.basename(args.guestPath);
      const isDir = (await m.exec(sh(`test -d ${q(args.guestPath)}`))).exitCode === 0;
      const tarSpec = isDir ? `-C ${q(args.guestPath)} .` : `-C ${q(parent)} ${q(base)}`;
      const result = await m.exec(sh(`tar -czf - ${tarSpec} | base64 | tr -d '\\n'`), { timeout: 600 });
      if (result.exitCode !== 0) throw new Error(`tar in machine failed: ${result.stderr}`);
      const archive = Buffer.from(result.stdout, "base64");
      if (archive.length > TRANSFER_CAP) throw new Error(`export exceeds ${TRANSFER_CAP} bytes`);
      const untar = await host("tar", ["-xzf", "-", "-C", args.hostPath], archive);
      if (untar.code !== 0) throw new Error(`tar on host failed: ${untar.stderr}`);
      return text({ machine: args.machine, from: args.guestPath, to: args.hostPath, bytes: archive.length });
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_list",
  {
    title: "List machines",
    description: "List machines on the current target with their state.",
    inputSchema: {},
  },
  async () => {
    try {
      const all = await Machine.list(conn);
      return text(all.map((m) => ({ machine: m.name, state: m.state, image: m.image, persistent: m.persistent })));
    } catch (error) {
      return failure(error);
    }
  },
);

server.registerTool(
  "smol_delete",
  {
    title: "Delete a machine",
    description:
      "Delete a machine or discard a branch. A source with live branches must have them deleted first.",
    inputSchema: { machine: z.string() },
  },
  async (args) => {
    try {
      const m = await machine(args.machine);
      await m.delete();
      machines.delete(args.machine);
      ephemeral.delete(args.machine);
      return text({ deleted: args.machine });
    } catch (error) {
      return failure(error);
    }
  },
);

// The client closing stdin ends the session: delete the machines it did not
// ask to keep, branches before their source, then exit.
let closing = false;
async function shutdown(code: number): Promise<void> {
  if (closing) return;
  closing = true;
  for (const name of [...ephemeral].reverse()) {
    try {
      await (await machine(name)).delete();
    } catch (error) {
      process.stderr.write(`smolmachines-mcp: could not delete ${name}: ${error instanceof Error ? error.message : String(error)}\n`);
    }
  }
  process.exit(code);
}
process.stdin.on("end", () => void shutdown(0));
process.on("SIGINT", () => void shutdown(130));
process.on("SIGTERM", () => void shutdown(143));

const transport = new StdioServerTransport();
await server.connect(transport);
