#!/usr/bin/env node
/**
 * smolmachines-mcp-http: the same server over Streamable HTTP, for an agent
 * that does not run where the machines do: host it inside a smol machine and
 * point the agent at the port the machine publishes.
 *
 * Each session gets its own stdio server (index.js) as a child process, so a
 * session ends the way a stdio client's does: its DELETE closes the child's
 * stdin, which deletes the machines it created with `persistent: false`. So
 * does this server exiting, since the child's stdin closes with it.
 *
 *   SMOL_MCP_AUTH_TOKEN=... npx -y -p smolmachines-mcp smolmachines-mcp-http [--host 127.0.0.1] [--port 8080] [--path /mcp]
 */

import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { ReadBuffer, serializeMessage } from "@modelcontextprotocol/sdk/shared/stdio.js";
import { spawn, type ChildProcess } from "node:child_process";
import { createHash, randomUUID, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage } from "node:http";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

function fail(message: string): never {
  process.stderr.write(`smolmachines-mcp-http: ${message}\n`);
  process.exit(1);
}

let flags: { host: string; port: string; path: string };
try {
  flags = parseArgs({
    options: {
      host: { type: "string", default: "127.0.0.1" },
      // What a smol machine publishes by default.
      port: { type: "string", default: "8080" },
      path: { type: "string", default: "/mcp" },
    },
  }).values as typeof flags;
} catch (error) {
  fail(error instanceof Error ? error.message : String(error));
}
const port = Number(flags.port);
if (!Number.isInteger(port) || port < 0 || port > 65535) fail(`--port must be a port number, got ${flags.port}`);

// The tools create machines and run commands in them, so a listener without a
// token would be a remote shell for anyone who can reach the port.
const token = process.env.SMOL_MCP_AUTH_TOKEN ?? "";
if (!token) fail("set SMOL_MCP_AUTH_TOKEN; every request must carry it");

const digest = (value: string) => createHash("sha256").update(value).digest();
const expected = digest(token);

/**
 * The token comes as `authorization: Bearer` or in `x-smol-mcp-token`: the
 * cloud ingress spends `authorization` on the account key and forwards it
 * unchanged, so a server hosted there cannot read its own token from it.
 */
function authorized(req: IncomingMessage): boolean {
  const alt = req.headers["x-smol-mcp-token"];
  const presented = typeof alt === "string" ? alt : /^Bearer (.+)$/i.exec(req.headers.authorization ?? "")?.[1];
  // Digests have one length, so the comparison time says nothing about the token.
  return presented !== undefined && timingSafeEqual(digest(presented), expected);
}

const SERVER = fileURLToPath(new URL("./index.js", import.meta.url));
const sessions = new Map<string, StreamableHTTPServerTransport>();

/** A session: the client's messages pass unchanged to and from its own stdio server. */
function session(): StreamableHTTPServerTransport {
  let child: ChildProcess | undefined;
  const transport = new StreamableHTTPServerTransport({
    sessionIdGenerator: randomUUID,
    // One JSON body per reply: no tool streams, and the reply may cross a proxy
    // whose buffering is not ours.
    enableJsonResponse: true,
    onsessioninitialized: (id) => {
      sessions.set(id, transport);
      const proc = spawn(process.execPath, [SERVER], { stdio: ["pipe", "pipe", "inherit"] });
      child = proc;
      const buffer = new ReadBuffer();
      proc.stdout!.on("data", (chunk: Buffer) => {
        buffer.append(chunk);
        try {
          for (let message; (message = buffer.readMessage()); ) void transport.send(message);
        } catch (error) {
          process.stderr.write(`smolmachines-mcp-http: session ${id}: ${error instanceof Error ? error.message : String(error)}\n`);
        }
      });
      // A write after the child exited fails with EPIPE; the exit closes the session.
      proc.stdin!.on("error", () => undefined);
      proc.on("exit", () => void transport.close());
    },
  });
  transport.onmessage = (message) => void child?.stdin!.write(serializeMessage(message));
  transport.onclose = () => {
    if (transport.sessionId) sessions.delete(transport.sessionId);
    child?.stdin!.end();
  };
  return transport;
}

const server = createServer(async (req, res) => {
  const reply = (status: number, message: string) =>
    res
      .writeHead(status, { "content-type": "application/json" })
      .end(JSON.stringify({ jsonrpc: "2.0", error: { code: -32000, message }, id: null }));
  if (new URL(req.url ?? "/", "http://localhost").pathname !== flags.path) return reply(404, "not found");
  if (!authorized(req)) return reply(401, "unauthorized: send the token as a bearer or in x-smol-mcp-token");
  const id = req.headers["mcp-session-id"];
  // A POST without a session may be an initialize; the transport rejects anything else.
  const transport = id === undefined ? (req.method === "POST" ? session() : undefined) : sessions.get(String(id));
  if (!transport) return reply(404, "unknown session");
  try {
    await transport.handleRequest(req, res);
  } catch (error) {
    process.stderr.write(`smolmachines-mcp-http: ${error instanceof Error ? error.message : String(error)}\n`);
    if (!res.headersSent) reply(500, "internal error");
  }
});
server.on("error", (error) => fail(error.message));
server.listen(port, flags.host, () => {
  const address = server.address();
  const bound = typeof address === "object" && address ? address.port : port;
  process.stderr.write(`smolmachines-mcp-http: listening on http://${flags.host}:${bound}${flags.path}\n`);
});
