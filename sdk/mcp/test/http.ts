/**
 * End-to-end test for smolmachines-mcp-http against the real local engine.
 *
 * Starts the built HTTP bin, drives it with the MCP client over Streamable
 * HTTP, and checks the rules a hosted server depends on: no token, no server;
 * no token on a request, nothing created; the token in either header; a
 * session's DELETE deletes what it did not ask to keep.
 *
 *   cd smol/sdk/mcp && npm install && npm run test:http
 *
 * Requires a host the engine supports (macOS Apple Silicon, or Linux + KVM).
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import path from "node:path";

let passed = 0;
let failed = 0;
function check(label: string, ok: boolean, detail = ""): void {
  if (ok) {
    passed++;
    console.log(`  ✓ ${label}`);
  } else {
    failed++;
    console.error(`  ✗ ${label}${detail ? `: ${detail}` : ""}`);
  }
}

type ToolResult = { content: Array<{ type: string; text?: string }>; isError?: boolean };

const BIN = path.resolve("dist/http.js");
const TOKEN = randomUUID();

function start(env: Record<string, string>) {
  return spawn(process.execPath, [BIN, "--port", "0"], {
    env: { ...process.env, SMOL_TARGET: "local", ...env },
    stdio: ["ignore", "inherit", "pipe"],
  });
}

async function main(): Promise<void> {
  console.log("smolmachines-mcp-http e2e\n");

  const bare = start({ SMOL_MCP_AUTH_TOKEN: "" });
  let bareErr = "";
  bare.stderr.on("data", (chunk) => (bareErr += chunk));
  const bareCode = await new Promise((resolve) => bare.on("exit", resolve));
  check("refuses to start without SMOL_MCP_AUTH_TOKEN", bareCode !== 0 && bareErr.includes("SMOL_MCP_AUTH_TOKEN"), `${bareCode} ${bareErr}`);

  const server = start({ SMOL_MCP_AUTH_TOKEN: TOKEN });
  const url = await new Promise<URL>((resolve, reject) => {
    let err = "";
    server.stderr.on("data", (chunk) => {
      process.stderr.write(chunk);
      err += chunk;
      const listening = /listening on (\S+)/.exec(err);
      if (listening) resolve(new URL(listening[1]));
    });
    server.on("exit", (code) => reject(new Error(`server exited with ${code}`)));
  });

  const connect = async (headers: Record<string, string>) => {
    const client = new Client({ name: "http-e2e", version: "0" });
    const transport = new StreamableHTTPClientTransport(url, { requestInit: { headers } });
    await client.connect(transport);
    const call = async (name: string, args: Record<string, unknown> = {}) => {
      const result = (await client.callTool({ name, arguments: args })) as ToolResult;
      const body = result.content.find((c) => c.type === "text")?.text ?? "";
      return { result, body, json: result.isError ? null : JSON.parse(body) };
    };
    return { client, transport, call };
  };
  const stamp = Date.now().toString(36);
  const named = async (call: Awaited<ReturnType<typeof connect>>["call"], prefix: string) =>
    ((await call("smol_list")).json as Array<{ machine: string }>).filter((m) => m.machine.startsWith(prefix));

  const a = await connect({ authorization: `Bearer ${TOKEN}` });
  try {
    // A session id alone must not be enough to drive the session.
    for (const [label, headers] of [
      ["no token", {}],
      ["wrong bearer", { authorization: "Bearer not-the-token" }],
      ["wrong x-smol-mcp-token", { "x-smol-mcp-token": "not-the-token" }],
    ] as const) {
      const response = await fetch(url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          accept: "application/json, text/event-stream",
          "mcp-session-id": a.transport.sessionId!,
          "mcp-protocol-version": a.transport.protocolVersion ?? "2025-03-26",
          ...headers,
        },
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 1,
          method: "tools/call",
          params: { name: "smol_create", arguments: { name: `http-e2e-${stamp}-unauth`, memoryMb: 512 } },
        }),
      });
      check(`${label}: 401`, response.status === 401, String(response.status));
    }
    check("a refused request creates nothing", (await named(a.call, `http-e2e-${stamp}-unauth`)).length === 0);

    // The cloud ingress hands the guest its account key in `authorization`.
    const b = await connect({ authorization: "Bearer smk-account-key", "x-smol-mcp-token": TOKEN });
    const tools = (await b.client.listTools()).tools.map((t) => t.name).sort();
    check("x-smol-mcp-token accepted beside another authorization", tools.length === 10 && tools.includes("smol_branch"), tools.join(","));

    const made = [
      await b.call("smol_create", { name: `http-e2e-${stamp}-eph`, persistent: false, memoryMb: 512 }),
      await b.call("smol_branch", { source: `http-e2e-${stamp}-eph`, name: `http-e2e-${stamp}-eph-b` }),
      await b.call("smol_create", { name: `http-e2e-${stamp}-keep`, memoryMb: 512 }),
    ];
    check(
      "session created its machines",
      made.every((r) => !r.result.isError) && (await named(a.call, `http-e2e-${stamp}`)).length === 3,
      made.map((r) => r.body.slice(0, 300)).join(" | "),
    );
    await b.transport.terminateSession();
    let left = await named(a.call, `http-e2e-${stamp}-eph`);
    for (const deadline = Date.now() + 30_000; left.length > 0 && Date.now() < deadline; ) {
      await new Promise((resolve) => setTimeout(resolve, 250));
      left = await named(a.call, `http-e2e-${stamp}-eph`);
    }
    check("session DELETE deletes its non-persistent machines and their branches", left.length === 0, JSON.stringify(left));
    check("session DELETE keeps its persistent machines", (await named(a.call, `http-e2e-${stamp}-keep`)).length === 1);
  } finally {
    for (const m of (await named(a.call, `http-e2e-${stamp}`)).reverse()) await a.call("smol_delete", { machine: m.machine });
    await a.client.close();
    server.kill("SIGTERM");
    await new Promise((resolve) => server.on("exit", resolve));
  }

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
