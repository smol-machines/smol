/** Real guest + published cloud HTTP transport, with a local control-plane bridge. */
import { strict as assert } from "node:assert";
import { execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import {
  createServer,
  type IncomingMessage,
  type ServerResponse,
} from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSandbox, type AgentProvider } from "@ai-hero/sandcastle";
import { Machine } from "smolmachines";
import { smol } from "./provider.js";

const readBody = async (req: IncomingMessage) => {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks);
};

const guest = await Machine.create(
  {
    image: "alpine/git:2.47.2",
    resources: {
      cpus: 1,
      memoryMb: 1024,
      allowHosts: [
        "registry-1.docker.io",
        "auth.docker.io",
        "production.cloudflare.docker.com",
        "index.docker.io",
      ],
    },
  },
  { target: "local" },
);
const host = await mkdtemp(join(tmpdir(), "sandcastle-smol-cloud-bridge-"));
let deleted = false;
const server = createServer(async (req, res: ServerResponse) => {
  try {
    assert.equal(req.headers.authorization, "Bearer test-token");
    const url = req.url ?? "";
    const result = (data: unknown, status = 200) => {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(status === 204 ? undefined : JSON.stringify(data));
    };
    if (req.method === "POST" && url === "/v1/machines") {
      return result(
        { id: "bridge-1", name: "bridge", state: "started", ready: true },
        201,
      );
    }
    if (req.method === "POST" && url === "/v1/machines/bridge-1/start") {
      return result({
        id: "bridge-1",
        name: "bridge",
        state: "started",
        ready: true,
      });
    }
    if (req.method === "GET" && url === "/v1/machines/bridge-1") {
      return result({
        id: "bridge-1",
        name: "bridge",
        state: "started",
        ready: true,
      });
    }
    if (req.method === "DELETE" && url === "/v1/machines/bridge-1") {
      deleted = true;
      await guest.delete();
      return result(undefined, 204);
    }
    const filePrefix = "/v1/machines/bridge-1/files/";
    if (url.startsWith(filePrefix)) {
      const path = decodeURIComponent(url.slice(filePrefix.length));
      if (req.method === "PUT") {
        await guest.writeFile(path, await readBody(req));
        return result(undefined, 204);
      }
      if (req.method === "GET") {
        const data = await guest.readFile(path);
        res.writeHead(200, { "content-type": "application/octet-stream" });
        return res.end(data);
      }
    }
    if (req.method === "POST" && url === "/v1/machines/bridge-1/exec") {
      const body = JSON.parse((await readBody(req)).toString()) as {
        command: string[];
        env?: Record<string, string>;
        cwd?: string;
        timeoutSeconds?: number;
      };
      const output = await guest.exec(body.command, {
        env: body.env,
        workdir: body.cwd ?? undefined,
        timeout: body.timeoutSeconds ?? undefined,
      });
      return result({
        exitCode: output.exitCode,
        stdout: output.stdout,
        stderr: output.stderr,
      });
    }
    if (req.method === "POST" && url === "/v1/machines/bridge-1/exec/stream") {
      const body = JSON.parse((await readBody(req)).toString()) as {
        command: string[];
        env?: Record<string, string>;
        cwd?: string;
      };
      res.writeHead(200, { "content-type": "text/event-stream" });
      for await (const event of guest.execStream(body.command, {
        env: body.env,
        workdir: body.cwd ?? undefined,
      })) {
        const data =
          event.kind === "exit"
            ? JSON.stringify({ exitCode: event.exitCode })
            : event.kind === "error"
              ? event.message
              : event.data;
        res.write(
          `event: ${event.kind}\n${data
            .split("\n")
            .map((line) => `data: ${line}`)
            .join("\n")}\n\n`,
        );
      }
      return res.end();
    }
    return result({ error: `${req.method} ${url}` }, 404);
  } catch (error) {
    res.destroy(error as Error);
  }
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const address = server.address();
assert.ok(address && typeof address !== "string");
const agent: AgentProvider = {
  name: "scripted",
  env: {},
  captureSessions: false,
  buildPrintCommand: () => ({
    command:
      "printf 'after\\n' > tracked.txt; printf 'new\\n' > created.txt; echo '<promise>COMPLETE</promise>'",
  }),
  parseStreamLine: (line) => [{ type: "text", text: line }],
};
try {
  const git = (...args: string[]) =>
    execFileSync("git", args, { cwd: host, stdio: "pipe" });
  git("init", "-b", "main");
  git("config", "user.name", "Example");
  git("config", "user.email", "example@example.com");
  await writeFile(join(host, "tracked.txt"), "before\n");
  git("add", "tracked.txt");
  git("commit", "-m", "Initial commit");
  console.log(
    "Creating Sandcastle sandbox through published cloud transport into a real local VM",
  );
  const sandbox = await createSandbox({
    branch: "smol-cloud-bridge-live",
    cwd: host,
    sandbox: smol({
      target: "cloud",
      cloud: {
        apiKey: "test-token",
        baseUrl: `http://127.0.0.1:${address.port}`,
      },
      image: "alpine/git:2.47.2",
      allowHosts: ["registry-1.docker.io", "api.anthropic.com"],
      cpus: 1,
      memoryMb: 1024,
    }),
  });
  try {
    assert.equal((await sandbox.exec("cat tracked.txt")).stdout, "before\n");
    const run = await sandbox.run({ agent, prompt: "Edit", maxIterations: 1 });
    assert.equal(run.completionSignal, "<promise>COMPLETE</promise>");
    assert.equal(
      await readFile(join(sandbox.worktreePath, "tracked.txt"), "utf8"),
      "after\n",
    );
    assert.equal(
      await readFile(join(sandbox.worktreePath, "created.txt"), "utf8"),
      "new\n",
    );
    console.log(
      "PASS: cloud SDK transport and Sandcastle Git sync on a real VM",
    );
  } finally {
    await sandbox.close();
  }
  assert.equal(deleted, true);
} finally {
  server.closeAllConnections();
  await new Promise<void>((resolve) => server.close(() => resolve()));
  if (!deleted) await guest.delete();
  await rm(host, { recursive: true, force: true });
}
