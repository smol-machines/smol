/** Exercise this adapter through the published SDK's real cloud HTTP transport. */
import { strict as assert } from "node:assert";
import {
  createServer,
  type IncomingMessage,
  type ServerResponse,
} from "node:http";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { IsolatedSandboxHandle } from "@ai-hero/sandcastle";
import { smol } from "./provider.js";

const body = async (req: IncomingMessage) => {
  const parts: Buffer[] = [];
  for await (const part of req) parts.push(Buffer.from(part));
  return Buffer.concat(parts);
};

test("cloud target provisions, streams output, moves large files, and deletes", async () => {
  const files = new Map<string, Buffer>();
  const hostDir = await mkdtemp(join(tmpdir(), "sandcastle-smol-cloud-"));
  let creation: any;
  let streamedOptions: any;
  let deleted = false;
  let sentExit = false;
  const server = createServer(async (req, res: ServerResponse) => {
    const url = req.url ?? "";
    assert.equal(req.headers.authorization, "Bearer test-token");
    const reply = (value: unknown, status = 200) => {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify(value));
    };
    try {
      if (req.method === "POST" && url === "/v1/machines") {
        creation = JSON.parse((await body(req)).toString());
        return reply({
          id: "mock-1",
          name: "mock-vm",
          state: "started",
          ready: true,
        });
      }
      if (req.method === "POST" && url === "/v1/machines/mock-1/start") {
        return reply({ id: "mock-1", state: "started", ready: true });
      }
      if (req.method === "GET" && url === "/v1/machines/mock-1") {
        return reply({ id: "mock-1", state: "started", ready: true });
      }
      if (req.method === "DELETE" && url === "/v1/machines/mock-1") {
        deleted = true;
        return reply(null, 204);
      }
      const filePrefix = "/v1/machines/mock-1/files/";
      if (url.startsWith(filePrefix)) {
        const path = decodeURIComponent(url.slice(filePrefix.length));
        if (req.method === "PUT") {
          files.set(path, await body(req));
          return reply(null, 204);
        }
        if (req.method === "GET") {
          const data = files.get(path);
          if (!data) return reply({ error: "not found" }, 404);
          res.writeHead(200, { "content-type": "application/octet-stream" });
          return res.end(data);
        }
      }
      if (req.method === "POST" && url === "/v1/machines/mock-1/exec") {
        const { command } = JSON.parse((await body(req)).toString()) as {
          command: string[];
        };
        if (command[0] === "sh" && command[2] === 'cat "$1" >> "$2"') {
          const [, , , , part, target] = command;
          files.set(
            target!,
            Buffer.concat([
              files.get(target!) ?? Buffer.alloc(0),
              files.get(part!)!,
            ]),
          );
        } else if (command[0] === "mv") {
          files.set(command[3]!, files.get(command[2]!)!);
          files.delete(command[2]!);
        } else if (command[0] === "rm") {
          files.delete(command[2]!);
        }
        return reply({ exitCode: 0, stdout: "", stderr: "" });
      }
      if (req.method === "POST" && url === "/v1/machines/mock-1/exec/stream") {
        streamedOptions = JSON.parse((await body(req)).toString());
        res.writeHead(200, { "content-type": "text/event-stream" });
        const inputPath = streamedOptions.command[5] as string | undefined;
        if (inputPath && files.has(inputPath)) {
          res.write(
            `event: stdout\ndata: ${files.get(inputPath)!.length}\ndata: \n\n`,
          );
        } else {
          res.write("event: stdout\ndata: first\ndata: \n\n");
          await new Promise((resolve) => setTimeout(resolve, 5));
          res.write("event: stdout\ndata: second\ndata: \n\n");
        }
        sentExit = true;
        res.end('event: exit\ndata: {"exitCode":0}\n\n');
        return;
      }
      return reply({ error: `${req.method} ${url}` }, 404);
    } catch (error) {
      res.destroy(error as Error);
    }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const provider = smol({
    target: "cloud",
    cloud: {
      apiKey: "test-token",
      baseUrl: `http://127.0.0.1:${address.port}`,
    },
    image: "alpine/git:2.47.2",
    allowHosts: ["registry-1.docker.io", "api.anthropic.com"],
  });
  // Sandcastle's published types mark create() internal, but its runtime uses it.
  const create = (
    provider as typeof provider & {
      create(options: {
        env: Record<string, string>;
      }): Promise<IsolatedSandboxHandle>;
    }
  ).create;
  let handle: IsolatedSandboxHandle | undefined;
  try {
    handle = await create({ env: { AGENT_TEST: "cloud" } });
    assert.equal(creation.source.reference, "alpine/git:2.47.2");
    assert.deepEqual(creation.network.hosts, [
      "registry-1.docker.io",
      "api.anthropic.com",
    ]);
    const lines: string[] = [];
    const result = await handle.exec("echo hi", {
      onLine: (line) => lines.push(line),
    });
    assert.deepEqual(lines, ["first", "second"]);
    assert.deepEqual(result, {
      stdout: "first\nsecond\n",
      stderr: "",
      exitCode: 0,
    });
    assert.equal(streamedOptions.env.AGENT_TEST, "cloud");
    assert.equal(streamedOptions.env.TMPDIR, "/workspace/.sandcastle-transfer");
    assert.equal(sentExit, true);

    const stdin = await handle.exec("wc -c", { stdin: "x".repeat(180_000) });
    assert.equal(stdin.stdout.trim(), "180000");
    const large = join(hostDir, "large.bundle");
    await writeFile(large, Buffer.alloc(9 * 1024 * 1024, 65));
    await handle.copyIn(large, "/workspace/sandcastle/large.bundle");
    assert.equal(
      files.get("/workspace/sandcastle/large.bundle")?.length,
      9 * 1024 * 1024,
    );
    const copied = join(hostDir, "copied.bundle");
    await handle.copyFileOut("/workspace/sandcastle/large.bundle", copied);
    assert.equal((await readFile(copied)).length, 9 * 1024 * 1024);
  } finally {
    try {
      await handle?.close();
    } finally {
      server.closeAllConnections();
      await new Promise<void>((resolve, reject) =>
        server.close((err) => (err ? reject(err) : resolve())),
      );
      await rm(hostDir, { recursive: true, force: true });
    }
  }
  assert.equal(deleted, true);
});
