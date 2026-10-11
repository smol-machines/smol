/** Contract checks against the published Strands SDK; no native engine required. */
import assert from "node:assert/strict";
import { Agent, SandboxAbortError, SandboxTimeoutError, type ToolContext } from "@strands-agents/sdk";
import { Machine } from "../dist/machine.js";
import type { ExecEvent, ExecOptions } from "../types";
import { SmolSandbox } from "../dist/strands.mjs";

async function main() {
  const calls: { command: string[]; options?: ExecOptions }[] = [];
  const files = new Map<string, Uint8Array>();
  let deletes = 0;
  const machine = {
    id: "borrowed-machine",
    async *execStream(command: string[], options?: ExecOptions): AsyncGenerator<ExecEvent> {
      calls.push({ command, options });
      if (command[2] === "wait") {
        yield { kind: "stdout", data: "partial" };
        await new Promise<void>((resolve, reject) => {
          const abort = () => reject(options?.signal?.reason);
          options?.signal?.addEventListener("abort", abort, { once: true });
        });
      }
      yield { kind: "stdout", data: "42" };
      yield { kind: "stderr", data: "warn" };
      yield { kind: "exit", exitCode: 0 };
    },
    readFile: async (path: string) => {
      const file = files.get(path);
      if (!file) throw new Error("missing file");
      return Buffer.from(file);
    },
    writeFile: async (path: string, content: Uint8Array) => { files.set(path, content); },
    delete: async () => { deletes++; },
  } as unknown as Machine;
  const sandbox = new SmolSandbox({ machine });
  assert.equal(await sandbox.machineId, "borrowed-machine");
  assert.deepEqual(sandbox.getTools().map((tool) => tool.name), ["sandbox_file_editor", "sandbox_shell"]);
  const chunks = [];
  for await (const chunk of sandbox.executeStreaming("echo 42", { cwd: "/workspace/dir", env: { ANSWER: "42" } })) {
    chunks.push(chunk);
  }
  assert.deepEqual(chunks, [
    { type: "streamChunk", streamType: "stdout", data: "42" },
    { type: "streamChunk", streamType: "stderr", data: "warn" },
    { type: "executionResult", stdout: "42", stderr: "warn", exitCode: 0, outputFiles: [] },
  ]);
  assert.deepEqual(calls[0], {
    command: ["sh", "-c", "echo 42"],
    options: { workdir: "/workspace/dir", env: { ANSWER: "42" }, signal: calls[0]?.options?.signal },
  });
  await assert.rejects(sandbox.execute("echo bad", { env: { "BAD=VAR": "value" } }), /Invalid environment/);
  await sandbox.writeFile("/workspace/a.bin", new Uint8Array([0, 255, 10]));
  assert.deepEqual(Array.from(await sandbox.readFile("/workspace/a.bin")), [0, 255, 10]);

  const abort = new AbortController();
  const aborted = sandbox.execute("wait", { signal: abort.signal });
  setTimeout(() => abort.abort(), 10);
  await assert.rejects(aborted, SandboxAbortError);
  await assert.rejects(sandbox.execute("wait", { timeout: 0.01 }), (error: unknown) => {
    assert(error instanceof SandboxTimeoutError);
    assert.equal(error.stdout, "partial");
    return true;
  });
  const agent = new Agent({ sandbox });
  await agent.initialize();
  assert.equal(agent.sandbox, sandbox);
  assert.deepEqual(agent.tools.filter((tool) => tool.name.startsWith("sandbox_")).map((tool) => tool.name),
    ["sandbox_file_editor", "sandbox_shell"]);
  const context: ToolContext = {
    toolUse: { name: "sandbox_shell", toolUseId: "test-tool", input: {} },
    agent,
    invocationState: {},
    cancelSignal: agent.cancelSignal,
    interrupt: () => { throw new Error("not interruptible"); },
  };
  const shell = agent.tools.find((tool) => tool.name === "sandbox_shell")!;
  const shellOutput = await shell.invoke({ command: "echo tool" }, context) as { output: string; error: string; exit_code: number };
  assert.deepEqual(shellOutput, { output: "42", error: "warn", exit_code: 0 });
  const editor = agent.tools.find((tool) => tool.name === "sandbox_file_editor")!;
  const created = await editor.invoke({ command: "create", path: "/workspace/agent.txt", file_text: "via Strands" }, context);
  assert.match(String(created), /created successfully/);
  assert.equal(new TextDecoder().decode(files.get("/workspace/agent.txt")), "via Strands");
  assert.equal(calls.find(({ command }) => command[2] === "echo tool")?.options?.timeout, 121,
    "Strands shell timeout must reach the API as a whole number of seconds");
  await sandbox.close();
  assert.equal(deletes, 0, "caller-owned machine must survive sandbox close");
  await assert.rejects(sandbox.execute("echo after close"), /closed/);

  const create = Machine.create;
  let createCount = 0;
  let deleted = 0;
  let shouldFailDelete = true;
  try {
    Machine.create = (async (config, conn) => {
      createCount++;
      assert.equal(conn?.target, "cloud");
      assert.equal(config?.image, "python:3.12-alpine");
      assert.equal(config?.resources?.network, false);
      assert.equal(config?.ttlSeconds, 3600);
      return {
        ...machine,
        delete: async () => {
          deleted++;
          if (shouldFailDelete) throw new Error("transient deletion failure");
        },
      } as unknown as Machine;
    }) as typeof Machine.create;
    const owned = new SmolSandbox({ conn: { target: "cloud" } });
    assert.equal(await owned.machineId, undefined, "VM creation must be lazy");
    await Promise.all([owned.execute("echo one"), owned.execute("echo two")]);
    assert.equal(createCount, 1, "concurrent commands must share a single VM");
    await assert.rejects(owned.close(), /transient deletion failure/);
    shouldFailDelete = false;
    await owned.close();
    await owned.close();
    assert.equal(deleted, 2, "failed cleanup must be retried exactly once");
    await assert.rejects(owned.execute("echo after close"), /closed/);
  } finally {
    Machine.create = create;
  }

  console.log("Strands published SDK: streaming, env, files, tools, abort, timeout, and ownership passed");
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
