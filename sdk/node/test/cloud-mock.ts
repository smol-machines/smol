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
import { Machine, NotSupportedError, SmolError } from "../index";

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
const readinessGets: Record<string, number> = {};

function readinessResponse(id: string, readyAfter: number, state = "started") {
  readinessGets[id] = (readinessGets[id] ?? 0) + 1;
  return {
    id,
    state,
    ready: readinessGets[id] >= readyAfter,
    readyAt:
      readinessGets[id] >= readyAfter ? "2026-07-22T20:01:41.152Z" : null,
  };
}

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
    return json(200, { state: "started", ready: false });
  }
  if (method === "POST" && url === "/v1/machines/m1/branches") {
    seen.forkBody = JSON.parse((await readBody(req)).toString() || "{}");
    if (seen.forkBody.name === "legacy-branch") {
      seen.newBranchReturned404 = true;
      return json(404, { code: "NOT_FOUND", error: "route not found" });
    }
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
  if (method === "POST" && url === "/v1/machines/m1/fork") {
    seen.legacyForkBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, {
      id: "m2",
      name: seen.legacyForkBody.name,
      state: "started",
      source: { type: "image", reference: "alpine" },
      resources: { cpus: 2, memoryMb: 1024 },
      network: { mode: "open" },
      env: {},
      ephemeral: false,
      ports: seen.legacyForkBody.ports ?? [],
    });
  }
  if (method === "POST" && url === "/v1/machines/m1/checkpoints") {
    seen.checkpointCreate = true;
    return json(201, {
      id: "ckpt-1",
      machineId: "m1",
      status: "available",
      sizeBytes: 4096,
      arch: "amd64",
      createdAt: "2026-08-26T00:00:00Z",
      downloadUrl: "/v1/checkpoints/ckpt-1/download",
    });
  }
  if (method === "GET" && url === "/v1/machines/m1/checkpoints") {
    return json(200, [{
      id: "ckpt-1",
      machineId: "m1",
      status: "available",
      sizeBytes: 4096,
      arch: "amd64",
      createdAt: "2026-08-26T00:00:00Z",
      downloadUrl: "/v1/checkpoints/ckpt-1/download",
    }]);
  }
  if (method === "POST" && url === "/v1/checkpoints/ckpt-1/restore") {
    seen.restoreBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, { id: "m-restored", name: seen.restoreBody.name, state: "stopped" });
  }
  if (method === "POST" && url === "/v1/machines/m-restored/start") {
    return json(200, { id: "m-restored", state: "started" });
  }
  if (method === "POST" && url === "/v1/machines/m1/branches/batch") {
    seen.forkBatchBody = JSON.parse((await readBody(req)).toString() || "{}");
    if (seen.forkBatchBody.namePrefix === "legacy") {
      seen.newBranchBatchReturned404 = true;
      return json(404, { code: "NOT_FOUND", error: "route not found" });
    }
    const n = seen.forkBatchBody.count ?? seen.forkBatchBody.names?.length ?? 0;
    const prefix = seen.forkBatchBody.namePrefix ?? "golden";
    const clones = Array.from({ length: n }, (_, i) => ({
      id: `b${i + 1}`,
      name: seen.forkBatchBody.names?.[i] ?? `${prefix}-${i + 1}`,
      state: "started",
      source: { type: "image", reference: "alpine" },
      resources: { cpus: 2, memoryMb: 1024 },
      network: { mode: "open" },
      env: {},
      ephemeral: false,
      ports: seen.forkBatchBody.ports ?? [],
    }));
    return json(201, { clones });
  }
  if (method === "POST" && url === "/v1/machines/m1/fork-batch") {
    seen.legacyForkBatchBody = JSON.parse((await readBody(req)).toString() || "{}");
    const n = seen.legacyForkBatchBody.count ?? 0;
    const prefix = seen.legacyForkBatchBody.namePrefix ?? "fork";
    return json(201, {
      clones: Array.from({ length: n }, (_, i) => ({
        id: `b${i + 1}`,
        name: `${prefix}-${i + 1}`,
        state: "started",
        source: { type: "image", reference: "alpine" },
        resources: { cpus: 2, memoryMb: 1024 },
        network: { mode: "open" },
        env: {},
        ephemeral: false,
        ports: [],
      })),
    });
  }
  if (method === "POST" && url === "/v1/machines/m1/assign") {
    seen.assignBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, {
      leaseId: seen.assignBody.leaseId,
      machineId: "ep1",
      ownerToken: "tok_secret",
      state: "ready",
      machine: {
        id: "ep1",
        name: "ep-1",
        state: "started",
        source: { type: "image", reference: "alpine" },
        resources: { cpus: 2, memoryMb: 1024 },
        network: { mode: "open" },
        env: {},
        ephemeral: false,
        ports: [],
      },
    });
  }
  if (method === "POST" && /^\/v1\/leases\/[^/]+\/heartbeat$/.test(url)) {
    seen.heartbeatBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, { leaseId: "task-99", machineId: "ep1", state: "ready" });
  }
  if (method === "POST" && /^\/v1\/leases\/[^/]+\/complete$/.test(url)) {
    seen.completeBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, { leaseId: "task-99", machineId: "ep1", state: "completed" });
  }
  if (method === "GET" && /^\/v1\/leases\/[^/]+$/.test(url)) {
    return json(200, {
      leaseId: "task-100",
      machineId: "ep1",
      state: "completed",
      reason: "done",
      score: 0.9,
      result: { passed: 3 },
    });
  }
  // Readiness for batch-fork clones (b1, b2, …) and the episode clone (ep1).
  if (method === "GET" && /^\/v1\/machines\/(b\d+|ep\d+)$/.test(url)) {
    return json(200, { id: url.split("/").pop(), state: "running" });
  }
  if (method === "GET" && url === "/v1/machines/m1")
    return json(200, readinessResponse("m1", 2));
  if (method === "GET" && url === "/v1/machines/m-restored")
    return json(200, { id: "m-restored", state: "running", ready: true });
  if (method === "GET" && url === "/v1/machines/m-wait")
    return json(200, readinessResponse("m-wait", 5));
  if (method === "GET" && url === "/v1/machines/m-timeout")
    return json(200, readinessResponse("m-timeout", Number.POSITIVE_INFINITY));
  if (method === "GET" && url === "/v1/machines/m-stopped")
    return json(200, readinessResponse("m-stopped", Number.POSITIVE_INFINITY, "stopped"));
  // The connect bridge: GET /v1/machines/:id/connect/:port[/rest]. Echo the
  // path + auth so the SDK's endpoint()/fetch() wiring can be asserted.
  if (method === "GET" && url.startsWith("/v1/machines/m1/connect/")) {
    seen.connectUrl = url;
    return json(200, { ok: true, path: url });
  }
  if (method === "GET" && url === "/v1/machines/m2")
    return json(200, { id: "m2", state: "running" });
  // Clone (fork) exec + delete — reward_fork forks m1 -> m2, grades in m2,
  // then deletes m2. Distinct stdout proves the GRADER ran in the clone, and
  // the DELETE proves the throwaway clone is always cleaned up.
  if (method === "POST" && url === "/v1/machines/m2/exec") {
    seen.rewardExecBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, {
      exitCode: 0,
      stdout: "reward-clone-exec-ok\n",
      stderr: "",
      stdoutTruncated: false,
      stderrTruncated: false,
    });
  }
  if (method === "DELETE" && url === "/v1/machines/m2") {
    seen.cloneDeleted = true;
    res.writeHead(204);
    return res.end();
  }
  if (method === "POST" && url === "/v1/machines/m1/exec") {
    seen.execBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, {
      exitCode: 0,
      stdout: "cloud-exec-ok\n",
      stderr: "",
      stdoutTruncated: true,
      stderrTruncated: false,
      // Byte-exact output includes a non-UTF-8 byte (0xFF) that the lossy text
      // field can't represent — proves the SDK decodes b64, not the text.
      stdoutB64: Buffer.from([0x68, 0x69, 0xff]).toString("base64"),
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
    "Machine.create() waited past started/ready=false",
    readinessGets.m1 >= 2,
    `${readinessGets.m1} readiness GET(s)`,
  );
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

  // Machine.connect() only attaches. Its caller must explicitly wait, and a
  // lifecycle state of "started" must not short-circuit ready=false.
  const connected = await Machine.connect("m-wait", {
    target: "cloud",
    baseUrl,
    apiKey: "smk_test123",
  });
  check(
    "connected started machine is not yet usable",
    (await connected.state()) === "started" && (await connected.ready()) === false,
  );
  const beforeWait = readinessGets["m-wait"];
  await connected.waitUntilReady({ timeoutMs: 500, intervalMs: 10 });
  check(
    "waitUntilReady() polls through started/ready=false",
    readinessGets["m-wait"] - beforeWait >= 2,
    `${readinessGets["m-wait"] - beforeWait} readiness GET(s)`,
  );

  const neverReady = await Machine.connect("m-timeout", {
    target: "cloud",
    baseUrl,
    apiKey: "smk_test123",
  });
  let timeoutError: unknown;
  try {
    await neverReady.waitUntilReady({ timeoutMs: 25, intervalMs: 5 });
  } catch (e) {
    timeoutError = e;
  }
  check(
    "readiness timeout reports machine, state, and duration",
    timeoutError instanceof SmolError &&
      timeoutError.code === "TIMEOUT" &&
      timeoutError.message.includes("m-timeout") &&
      timeoutError.message.includes("25ms") &&
      timeoutError.message.includes("state=started"),
    String(timeoutError),
  );

  const stopped = await Machine.connect("m-stopped", {
    target: "cloud",
    baseUrl,
    apiKey: "smk_test123",
  });
  let terminalError: unknown;
  try {
    await stopped.waitUntilReady({ timeoutMs: 100, intervalMs: 5 });
  } catch (e) {
    terminalError = e;
  }
  check(
    "terminal state fails readiness with a useful error",
    terminalError instanceof SmolError &&
      terminalError.message.includes("m-stopped") &&
      terminalError.message.includes("stopped before becoming ready"),
    String(terminalError),
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
    "exec exposes byte-exact stdoutBytes from base64",
    Buffer.from(r.stdoutBytes).equals(Buffer.from([0x68, 0x69, 0xff])),
    Buffer.from(r.stdoutBytes).toString("hex"),
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

  let syncGated = false;
  try {
    await m.sync();
  } catch (e) {
    syncGated = e instanceof NotSupportedError;
  }
  check("cloud sync is gated as NotSupported", syncGated);

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

  // --- branch: live-RAM child over the cloud ---
  const clone = await m.branch("rollout-1", {
    ports: [{ host: 18080, guest: 80 }],
    branchable: true,
  });

  const checkpoint = await m.checkpoint();
  check(
    "checkpoint captures durable live state",
    seen.checkpointCreate === true && checkpoint.id === "ckpt-1" && checkpoint.sizeBytes === 4096,
    JSON.stringify(checkpoint),
  );
  const checkpoints = await m.checkpoints();
  check("checkpoints lists captured state", checkpoints.length === 1 && checkpoints[0].arch === "amd64");
  const restored = await Machine.restoreCheckpoint(
    "ckpt-1",
    "restored",
    { target: "cloud", baseUrl, apiKey: "smk_test123" },
  );
  check(
    "restoreCheckpoint returns a ready machine",
    seen.restoreBody?.name === "restored" && restored.id === "m-restored" && await restored.ready(),
    JSON.stringify(seen.restoreBody),
  );
  check(
    "branch uses POST /branches with the child name",
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
    "branch can promote the child to another branch source",
    seen.forkBody?.branchable === true,
    JSON.stringify(seen.forkBody),
  );
  check(
    "branch returns a running child handle",
    clone.name === "rollout-1" && (await clone.state()) === "running",
    clone.name,
  );
  const legacyBranch = await m.branch("legacy-branch", { branchable: true });
  check(
    "branch falls back to a legacy /fork control plane",
    seen.newBranchReturned404 === true &&
      seen.legacyForkBody?.name === "legacy-branch" &&
      seen.legacyForkBody?.forkable === true &&
      legacyBranch.name === "legacy-branch",
    JSON.stringify(seen.legacyForkBody),
  );

  // --- branch batch: fan out N children in one transactional call ---
  const batch = await m.branchBatch({ count: 3, namePrefix: "rollout" });
  check(
    "branchBatch uses POST /branches/batch with the size spec",
    seen.forkBatchBody?.count === 3 &&
      seen.forkBatchBody?.namePrefix === "rollout",
    JSON.stringify(seen.forkBatchBody),
  );
  check(
    "branchBatch returns N child handles in request order",
    batch.length === 3 &&
      batch[0].name === "rollout-1" &&
      batch[2].name === "rollout-3",
    batch.map((c) => c.name).join(","),
  );
  const legacyBatch = await m.branchBatch({ count: 2, namePrefix: "legacy" });
  check(
    "branchBatch falls back to a legacy /fork-batch control plane",
    seen.newBranchBatchReturned404 === true &&
      seen.legacyForkBatchBody?.count === 2 &&
      legacyBatch.length === 2,
    JSON.stringify(seen.legacyForkBatchBody),
  );

  // --- assign: lease an RL episode, heartbeat, complete ---
  const episode = await m.assign({
    leaseId: "task-99",
    task: { seed: 7 },
    secrets: { KEY: "v" },
  });
  check(
    "assign hit POST /assign with leaseId + task + secrets",
    seen.assignBody?.leaseId === "task-99" &&
      JSON.stringify(seen.assignBody?.task) === JSON.stringify({ seed: 7 }) &&
      JSON.stringify(seen.assignBody?.secrets) === JSON.stringify({ KEY: "v" }),
    JSON.stringify(seen.assignBody),
  );
  check(
    "episode exposes the provisioned clone",
    episode.machine.name === "ep-1",
    episode.machine.name,
  );
  await episode.heartbeat();
  check(
    "heartbeat sent the owner token to /leases/:id/heartbeat",
    seen.heartbeatBody?.ownerToken === "tok_secret",
    JSON.stringify(seen.heartbeatBody),
  );
  await episode.complete("done");
  check(
    "complete sent owner token + reason to /leases/:id/complete",
    seen.completeBody?.ownerToken === "tok_secret" && seen.completeBody?.reason === "done",
    JSON.stringify(seen.completeBody),
  );

  // --- Machine.id + Machine.start (resume a stopped machine) ---
  check("machine exposes its id", m.id === "m1", m.id);
  await m.start(); // POST /start + wait-ready (both mocked) — must not throw
  check("start() resumes a stopped machine without error", true);

  // --- complete with score/result, read the outcome back via status() ---
  const episode2 = await m.assign({ leaseId: "task-100" });
  await episode2.complete("done", { score: 0.9, result: { passed: 3 } });
  check(
    "complete sent score + result",
    seen.completeBody?.score === 0.9 &&
      JSON.stringify(seen.completeBody?.result) === JSON.stringify({ passed: 3 }),
    JSON.stringify(seen.completeBody),
  );
  const st = await episode2.status();
  check(
    "status() reads the lease outcome (state + score)",
    st.state === "completed" && st.score === 0.9,
    JSON.stringify(st),
  );

  // --- reward_fork: grade in a throwaway clone without touching the original ---
  seen.cloneDeleted = false;
  const reward = await m.rewardFork(["python", "grade.py"]);
  check(
    "reward_fork grades in the CLONE (not the parent)",
    reward.stdout.trim() === "reward-clone-exec-ok",
    reward.stdout,
  );
  check(
    "reward_fork sent the grader command to the clone",
    JSON.stringify(seen.rewardExecBody?.command) ===
      JSON.stringify(["python", "grade.py"]),
    JSON.stringify(seen.rewardExecBody),
  );
  check(
    "reward_fork destroys the throwaway clone (cleanup)",
    seen.cloneDeleted === true,
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
