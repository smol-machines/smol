/**
 * Cloud-transport test against a localhost mock of the smolfleet `/v1` API.
 *
 * Verifies the CloudTransport wiring — request paths, Bearer auth, JSON/byte
 * round-trips, and capability gating — WITHOUT needing the real cloud.
 *
 *   npx tsx test/cloud-mock.ts
 */

import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { Machine, NotSupportedError } from "../index";

let passed = 0;
let failed = 0;
const check = (label: string, ok: boolean, detail = "") => {
  if (ok) {
    passed++;
    console.log(`  ✓ ${label}`);
  } else {
    failed++;
    console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ""}`);
  }
};

// --- in-memory mock cloud ---
const seen: any = { auth: null, execBody: null };
const files = new Map<string, Buffer>();

function readBody(req: any): Promise<Buffer> {
  return new Promise((resolve) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
  });
}

const server = createServer(async (req, res) => {
  const url = req.url ?? "";
  const method = req.method ?? "GET";
  seen.auth = req.headers["authorization"] ?? seen.auth;
  // The real control plane sets x-request-id on every response; mirror it so the
  // SDK's error-message surfacing can be asserted.
  res.setHeader("x-request-id", "req-test-abc");
  const json = (code: number, obj: unknown) => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(obj));
  };

  if (method === "POST" && url === "/v1/machines") {
    seen.createBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, { id: "m1", name: "cloud-test", state: "created" });
  }
  if (method === "POST" && url.startsWith("/v1/machines/m1/start")) {
    seen.startUrl = url;
    return json(200, { state: "running" });
  }
  if (method === "POST" && url === "/v1/machines/m1/fork") {
    seen.forkBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, {
      id: "m2",
      name: seen.forkBody.name ?? "clone",
      state: "started",
      source: { type: "image", reference: "alpine" },
      resources: { cpus: 2, memoryMb: 1024 },
      network: { mode: "open" },
      env: {},
      ephemeral: false,
      ports: seen.forkBody.ports ?? [],
    });
  }
  if (method === "GET" && url === "/v1/machines/m1")
    return json(200, {
      id: "m1",
      state: "started",
      ready: true,
      readyAt: "2026-07-22T20:01:41.152Z",
    });
  // The connect bridge: GET /v1/machines/:id/connect/:port[/rest]. Echo the
  // path + auth so the SDK's endpoint()/fetch() wiring can be asserted.
  if (method === "GET" && url.startsWith("/v1/machines/m1/connect/")) {
    seen.connectUrl = url;
    return json(200, { ok: true, path: url });
  }
  if (method === "GET" && url === "/v1/machines/m2")
    return json(200, { id: "m2", state: "running" });
  if (method === "POST" && url === "/v1/machines/m1/exec") {
    seen.execBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, {
      exitCode: 0,
      stdout: "cloud-exec-ok\n",
      stderr: "",
      stdoutTruncated: true,
      stderrTruncated: false,
    });
  }
  if (method === "PUT" && url.startsWith("/v1/machines/m1/files/")) {
    files.set(url, await readBody(req));
    res.writeHead(204);
    return res.end();
  }
  if (method === "GET" && url.startsWith("/v1/machines/m1/files/")) {
    const b = files.get(url);
    if (!b) {
      res.writeHead(404);
      return res.end();
    }
    res.writeHead(200, { "content-type": "application/octet-stream" });
    return res.end(b);
  }
  if (method === "POST" && url === "/v1/machines/m1/stop")
    return json(200, { state: "stopped" });
  if (method === "DELETE" && url === "/v1/machines/m1") {
    res.writeHead(204);
    return res.end();
  }
  res.writeHead(404);
  res.end("no route");
});

async function main(): Promise<void> {
  console.log("smol SDK cloud-transport test (mock /v1)\n");
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as AddressInfo).port;
  const baseUrl = `http://127.0.0.1:${port}`;

  const m = await Machine.create(
    {
      image: "alpine",
      forkable: true,
      env: { FOO: "bar" },
      workdir: "/app",
      resources: { cpus: 2, memoryMb: 1024 },
    },
    { target: "cloud", baseUrl, apiKey: "smk_test123" },
  );
  check("created via cloud (name from API)", m.name === "cloud-test", m.name);
  check(
    "create sends env as a plain map + workdir",
    JSON.stringify(seen.createBody?.env) === JSON.stringify({ FOO: "bar" }) &&
      seen.createBody?.workdir === "/app",
    JSON.stringify({ env: seen.createBody?.env, workdir: seen.createBody?.workdir }),
  );
  check(
    "sent Bearer auth",
    seen.auth === "Bearer smk_test123",
    String(seen.auth),
  );
  check("state() over REST", (await m.state()) === "started");
  // Readiness: the machine can be `started` yet report `ready` separately — the
  // SDK surfaces the unambiguous signal (gate on this, not state).
  check("ready() reads the readiness flag", (await m.ready()) === true);
  check(
    "readyAt() reads the readiness timestamp",
    (await m.readyAt()) === "2026-07-22T20:01:41.152Z",
    String(await m.readyAt()),
  );
  await m.waitUntilReady({ timeoutMs: 2000, intervalMs: 50 });
  check("waitUntilReady() resolves on ready", true);
  check(
    "forkable start passes ?forkable=true",
    String(seen.startUrl ?? "").includes("forkable=true"),
    String(seen.startUrl),
  );

  // --- connect bridge: authed endpoint URL + fetch to a published guest port ---
  const ep = m.endpoint(80);
  check(
    "endpoint() builds the connect-bridge httpUrl",
    ep.httpUrl === `${baseUrl}/v1/machines/m1/connect/80`,
    ep.httpUrl,
  );
  check(
    "endpoint() derives a wss/ws URL from the base",
    ep.wsUrl === `${baseUrl.replace(/^http/, "ws")}/v1/machines/m1/connect/80`,
    ep.wsUrl,
  );
  check(
    "endpoint() carries the Bearer auth header",
    ep.headers.authorization === "Bearer smk_test123",
    ep.headers.authorization,
  );
  check(
    "endpoint(port, path) appends the sub-path",
    m.endpoint(80, "/healthz").httpUrl ===
      `${baseUrl}/v1/machines/m1/connect/80/healthz`,
    m.endpoint(80, "/healthz").httpUrl,
  );
  const bridged = await m.fetch(80, "healthz");
  const bridgedBody = (await bridged.json()) as { ok?: boolean; path?: string };
  check(
    "fetch() reaches the guest port through the authed bridge",
    bridged.ok &&
      bridgedBody.ok === true &&
      seen.connectUrl === "/v1/machines/m1/connect/80/healthz",
    String(seen.connectUrl),
  );

  const r = await m.exec(["echo", "hi"], { env: { A: "b" }, timeout: 5 });
  check("exec stdout mapped", r.stdout.trim() === "cloud-exec-ok");
  check(
    "exec surfaces truncation flags",
    r.stdoutTruncated === true && r.stderrTruncated === false,
    `${r.stdoutTruncated}/${r.stderrTruncated}`,
  );
  check(
    "exec sent command array",
    JSON.stringify(seen.execBody?.command) === JSON.stringify(["echo", "hi"]),
  );
  check(
    "exec sent env + timeoutSeconds",
    seen.execBody?.env?.A === "b" && seen.execBody?.timeoutSeconds === 5,
  );

  await m.writeFile("/tmp/x", "cloud-rt");
  const back = await m.readFile("/tmp/x");
  check(
    "file round-trip over REST",
    back.toString() === "cloud-rt",
    back.toString(),
  );

  let runGated = false;
  try {
    await m.run("alpine", ["echo", "x"]);
  } catch (e) {
    runGated = e instanceof NotSupportedError;
  }
  check("run() gated as NotSupported on cloud", runGated);

  let mountsGated = false;
  try {
    await Machine.create(
      { image: "alpine", mounts: [{ source: "/data", target: "/data" }] },
      { target: "cloud", baseUrl, apiKey: "smk_test123" },
    );
  } catch (e) {
    mountsGated = e instanceof NotSupportedError;
  }
  check("cloud create rejects host mounts as NotSupported", mountsGated);

  // Published ports ARE a cloud feature: create sends only the guest port; the
  // control plane allocates the node host port. (Contrast: host mounts above.)
  await Machine.create(
    { image: "alpine", ports: [{ host: 8080, guest: 80 }] },
    { target: "cloud", baseUrl, apiKey: "smk_test123" },
  );
  check(
    "cloud create publishes ports (guest port only; hostPort allocated)",
    JSON.stringify(seen.createBody?.ports) === JSON.stringify([{ port: 80 }]),
    JSON.stringify(seen.createBody?.ports),
  );
  check(
    "env/workdir omitted from the body when unset",
    !("env" in (seen.createBody ?? {})) && !("workdir" in (seen.createBody ?? {})),
    JSON.stringify(seen.createBody),
  );

  // --- fork: live-RAM RL clone over the cloud ---
  const clone = await m.fork("rollout-1", [{ host: 18080, guest: 80 }]);
  check(
    "fork hit POST /fork with clone name",
    seen.forkBody?.name === "rollout-1",
    JSON.stringify(seen.forkBody),
  );
  check(
    "fork ports mapped guest+hostPort",
    JSON.stringify(seen.forkBody?.ports) ===
      JSON.stringify([{ port: 80, hostPort: 18080 }]),
    JSON.stringify(seen.forkBody?.ports),
  );
  check(
    "fork returns running clone handle",
    clone.name === "rollout-1" && (await clone.state()) === "running",
    clone.name,
  );

  // Errors surface the server's x-request-id so support can correlate the call
  // (clients see the error body but not response headers).
  let ridErrMsg = "";
  try {
    await m.readFile("/does-not-exist");
  } catch (e) {
    ridErrMsg = String((e as Error).message);
  }
  check(
    "error message surfaces x-request-id",
    ridErrMsg.includes("[request id: req-test-abc]"),
    ridErrMsg,
  );

  await m.stop();
  await m.delete();
  check("stop + delete over REST (no throw)", true);

  console.log(`\n${passed} passed, ${failed} failed`);
  server.close();
  if (failed > 0) process.exit(1);
}

main().catch((e) => {
  console.error("cloud-mock crashed:", e);
  server.close();
  process.exit(1);
});
