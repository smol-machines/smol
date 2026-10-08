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
import { CacheDisk, Machine, NotSupportedError, SmolError } from "../index";

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
const seen: any = { auth: null, execBody: null, execBodies: [] as any[] };
// Whether the mock control plane echoes a per-command `user` (a current one
// does; one that predates the field silently drops it and echoes nothing).
let echoUser = true;
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

  if (method === "PUT" && url.startsWith("/v1/credentials/")) {
    seen.credentialPuts = seen.credentialPuts ?? [];
    seen.credentialPuts.push({
      name: decodeURIComponent(url.slice("/v1/credentials/".length)),
      body: JSON.parse((await readBody(req)).toString() || "{}"),
    });
    return json(200, { name: "notion", envVar: "NOTION_API_KEY", hosts: [], createdAt: "", updatedAt: "" });
  }
  const cacheDiskInfo = (version: number) => ({
    id: "cdisk-1",
    name: "deps",
    sizeGb: 20,
    mountPath: "/cache",
    latestVersion: version,
    versions: [{ version, sizeBytes: 262144, sha256: "ab".repeat(32), createdAt: "" }],
    createdAt: "",
  });
  if (method === "POST" && url === "/v1/cache-disks") {
    seen.cacheDiskCreate = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, cacheDiskInfo(0));
  }
  if (method === "GET" && url === "/v1/cache-disks") {
    return json(200, { cacheDisks: [cacheDiskInfo(0)] });
  }
  if (method === "GET" && url.startsWith("/v1/cache-disks/")) {
    seen.cacheDiskGet = decodeURIComponent(url.slice("/v1/cache-disks/".length));
    return json(200, cacheDiskInfo(0));
  }
  if (method === "DELETE" && url.startsWith("/v1/cache-disks/")) {
    seen.cacheDiskDelete = decodeURIComponent(url.slice("/v1/cache-disks/".length));
    res.statusCode = 204;
    return res.end();
  }
  if (method === "POST" && /^\/v1\/machines\/[^/]+\/cache-disk\/publish$/.test(url)) {
    seen.cacheDiskPublish = url;
    return json(200, { cacheDisk: cacheDiskInfo(1), version: cacheDiskInfo(1).versions[0] });
  }
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
  if (method === "POST" && url === "/v1/checkpoints") {
    seen.uploadBody = JSON.parse((await readBody(req)).toString() || "{}");
    const base = `http://${req.headers.host}`;
    return json(201, {
      checkpoint: { id: "ckpt-up", machineId: "", status: "uploading", sizeBytes: seen.uploadBody.sizeBytes, arch: "", createdAt: "2026-10-04T00:00:00Z" },
      partSizeBytes: 6,
      uploadUrls: [`${base}/upload/1`, `${base}/upload/2`],
      expiresAt: "2026-10-04T06:00:00Z",
    });
  }
  if (method === "PUT" && url.startsWith("/upload/")) {
    seen.uploadAuth = req.headers["authorization"] ?? null;
    files.set(url, await readBody(req));
    res.writeHead(200);
    return res.end();
  }
  if (method === "POST" && url === "/v1/checkpoints/ckpt-up/complete") {
    seen.uploaded = Buffer.concat([files.get("/upload/1") ?? Buffer.alloc(0), files.get("/upload/2") ?? Buffer.alloc(0)]);
    return json(200, { id: "ckpt-up", machineId: "", status: "available", sizeBytes: seen.uploaded.length, arch: "arm64", createdAt: "2026-10-04T00:00:00Z" });
  }
  if (method === "POST" && url === "/v1/checkpoints/ckpt-up/restore") {
    seen.uploadRestoreBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, { id: "m-restored", name: seen.uploadRestoreBody.name, state: "stopped" });
  }
  if (method === "POST" && url === "/v1/checkpoints/ckpt-1/restore") {
    seen.restoreBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(201, { id: "m-restored", name: seen.restoreBody.name, state: "stopped" });
  }
  if (method === "POST" && url === "/v1/machines/m-restored/start") {
    return json(200, { id: "m-restored", state: "started" });
  }
  if (method === "POST" && url === "/v1/machines/m1/resize") {
    seen.resizeBody = JSON.parse((await readBody(req)).toString() || "{}");
    return json(200, {
      id: "m1",
      name: "golden",
      state: "started",
      resources: {
        cpus: seen.resizeBody.cpus ?? 2,
        memoryMb: seen.resizeBody.memoryMb ?? 1024,
        diskGb: seen.resizeBody.diskGb ?? null,
      },
      ports: [],
    });
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
    seen.execBodies.push(seen.execBody);
    return json(200, {
      ...(echoUser && seen.execBody.user !== undefined ? { user: seen.execBody.user } : {}),
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
  if (method === "POST" && url === "/v1/machines/m1/share")
    return json(200, {
      token: "msh_tok123",
      url: "https://app-abc.apps.smolmachines.com?t=msh_tok123",
    });
  // A tenant with no apps domain (or a name that is not DNS-safe) gets a token
  // and no URL; the SDK must surface null rather than inventing one.
  if (method === "POST" && url === "/v1/machines/m2/share")
    return json(200, { token: "msh_tok456" });
  if (method === "DELETE" && url === "/v1/checkpoints/ckpt-old") {
    seen.deletedCheckpoint = true;
    return json(204, {});
  }
  if (method === "DELETE" && url === "/v1/machines/m1/share") {
    seen.unshared = true;
    res.writeHead(204);
    return res.end();
  }
  if (method === "POST" && url === "/v1/machines/m1/stop")
    return json(200, { state: "stopped" });
  if (method === "POST" && url === "/v1/machines/m1/pause")
    return json(200, { state: "paused" });
  if (method === "POST" && url === "/v1/machines/m1/resume")
    return json(200, { state: "started" });
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

  // A credential with a value is stored for the account first; the machine
  // then binds every credential by name, including one already stored.
  seen.credentialPuts = [];
  await Machine.create(
    {
      image: "alpine",
      credentials: [
        { name: "notion", envVar: "NOTION_API_KEY", hosts: ["api.notion.com", "files.notion.com"], value: "secret_x" },
        { name: "github", envVar: "GITHUB_TOKEN", hosts: ["api.github.com"] },
      ],
    },
    { target: "cloud", baseUrl, apiKey: "smk_test123" },
  );
  check(
    "cloud create stores a credential that carries a value",
    JSON.stringify(seen.credentialPuts) ===
      JSON.stringify([
        {
          name: "notion",
          body: { envVar: "NOTION_API_KEY", hosts: ["api.notion.com", "files.notion.com"], value: "secret_x" },
        },
      ]),
    JSON.stringify(seen.credentialPuts),
  );
  check(
    "cloud create binds every credential by name",
    JSON.stringify(seen.createBody?.credentials) === JSON.stringify(["notion", "github"]),
    JSON.stringify(seen.createBody?.credentials),
  );
  let methodsRefused = false;
  seen.credentialPuts = [];
  try {
    await Machine.create(
      { image: "alpine", credentials: [{ name: "gh", envVar: "GH", hosts: ["api.github.com"], methods: ["GET"], value: "v" }] },
      { target: "cloud", baseUrl, apiKey: "smk_test123" },
    );
  } catch (e) {
    methodsRefused = e instanceof NotSupportedError;
  }
  check(
    "a method restriction is refused on the cloud before anything is stored",
    methodsRefused && seen.credentialPuts.length === 0,
    `${methodsRefused} ${JSON.stringify(seen.credentialPuts)}`,
  );

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

  // The control plane opens egress when `network` is absent, so an explicit
  // "no network" must be sent as `blocked`, and unset must stay absent.
  await Machine.create(
    { image: "alpine", resources: { network: false } },
    { target: "cloud", baseUrl, apiKey: "smk_test123" },
  );
  check(
    "network: false is sent as blocked",
    JSON.stringify(seen.createBody?.network) === JSON.stringify({ mode: "blocked" }),
    JSON.stringify(seen.createBody?.network),
  );
  await Machine.create(
    { image: "alpine", network: true },
    { target: "cloud", baseUrl, apiKey: "smk_test123" },
  );
  check(
    "network: true is sent as open",
    JSON.stringify(seen.createBody?.network) === JSON.stringify({ mode: "open" }),
    JSON.stringify(seen.createBody?.network),
  );
  await Machine.create({ image: "alpine" }, { target: "cloud", baseUrl, apiKey: "smk_test123" });
  check(
    "unset network is left to the control plane",
    !("network" in (seen.createBody ?? {})),
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
  {
    const { mkdtempSync, writeFileSync } = await import("node:fs");
    const { tmpdir } = await import("node:os");
    const { join } = await import("node:path");
    const file = join(mkdtempSync(join(tmpdir(), "smol-upload-")), "laptop.checkpoint");
    writeFileSync(file, Buffer.from("0123456789"));
    let lastProgress = [0, 0];
    const info = await Machine.uploadCheckpoint(
      file,
      { target: "cloud", baseUrl, apiKey: "smk_test123" },
      (sent, total) => (lastProgress = [sent, total]),
    );
    check(
      "uploadCheckpoint sends each slice to its URL and completes",
      info.id === "ckpt-up" && seen.uploaded?.toString() === "0123456789" && seen.uploadBody?.sizeBytes === 10,
      `${seen.uploaded} ${JSON.stringify(seen.uploadBody)}`,
    );
    check("...without sending the API key to storage", seen.uploadAuth === null, String(seen.uploadAuth));
    check("...and reports progress to the end", lastProgress[0] === 10 && lastProgress[1] === 10, String(lastProgress));
    files.clear();
    const moved = await Machine.restoreCheckpoint(
      file,
      "moved",
      { target: "cloud", baseUrl, apiKey: "smk_test123" },
      { networkPolicy: "allow-all" },
    );
    check(
      "restoring a local file in the cloud uploads it and keeps the network policy",
      moved.id === "m-restored" &&
        seen.uploaded?.toString() === "0123456789" &&
        seen.uploadRestoreBody?.name === "moved" &&
        seen.uploadRestoreBody?.network?.mode === "open",
      JSON.stringify(seen.uploadRestoreBody),
    );
  }
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
  check(
    "branch leaves the source running by default",
    seen.forkBody?.freezeSource === undefined,
    JSON.stringify(seen.forkBody),
  );
  await m.branch("frozen-1", { freezeSource: true });
  check(
    "branch({ freezeSource: true }) asks to keep the source as a branch base",
    seen.forkBody?.freezeSource === true,
    JSON.stringify(seen.forkBody),
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

  // --- delete a stored checkpoint by id ---
  await Machine.deleteCheckpoint("ckpt-old", { target: "cloud", baseUrl, apiKey: "smk_test123" });
  check("deleteCheckpoint sends DELETE /v1/checkpoints/<id>", seen.deletedCheckpoint === true);

  // --- resize: grow the running machine in place ---
  const grown = await m.resize({ cpus: 4, memoryMb: 4096, storageGb: 40 });
  check(
    "resize sends the cloud field names (diskGb for the one disk)",
    JSON.stringify(seen.resizeBody) === JSON.stringify({ cpus: 4, memoryMb: 4096, diskGb: 40 }),
    JSON.stringify(seen.resizeBody),
  );
  check(
    "resize returns the new size",
    grown.cpus === 4 && grown.memoryMb === 4096 && grown.storageGb === 40,
    JSON.stringify(grown),
  );
  let overlayRefused = false;
  try {
    await m.resize({ overlayGb: 10 });
  } catch (e) {
    overlayRefused = e instanceof SmolError;
  }
  check("resize refuses overlayGb on the cloud", overlayRefused);

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
  check(
    "branchBatch leaves the source running by default",
    seen.forkBatchBody?.freezeSource === undefined,
    JSON.stringify(seen.forkBatchBody),
  );
  await m.branchBatch({ count: 2, namePrefix: "frozen", freezeSource: true });
  check(
    "branchBatch({ freezeSource: true }) asks to keep the source as a branch base",
    seen.forkBatchBody?.freezeSource === true,
    JSON.stringify(seen.forkBatchBody),
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
  await m.pause();
  await m.resume();
  check("pause/resume use their explicit REST routes", true);

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

  const link = await m.share();
  check(
    "share() returns the token and the ready-to-use URL",
    link.token === "msh_tok123" &&
      link.url === "https://app-abc.apps.smolmachines.com?t=msh_tok123",
    JSON.stringify(link),
  );
  // No apps domain configured: the control plane sends a token and no url, and
  // the SDK must report null rather than fabricating a URL.
  const m2 = await Machine.connect("m2", {
    target: "cloud",
    baseUrl,
    apiKey: "smk_test123",
  });
  const bare = await m2.share();
  check(
    "share() reports url=null when the control plane omits it",
    bare.token === "msh_tok456" && bare.url === null,
    JSON.stringify(bare),
  );
  await m.unshare();
  check("unshare() issues DELETE on the share route", seen.unshared === true);

  // --- per-command user: probe the control plane before running as a user ---
  const cloudConn = { target: "cloud" as const, baseUrl, apiKey: "smk_test123" };
  seen.execBodies = [];
  const asUser = await m.exec(["id", "-u"], { user: "nobody" });
  const firstRun = seen.execBodies.map((b: any) => [b.command.join(" "), b.user]);
  check(
    "a user-bearing exec probes with a harmless `true` first, then runs as the user",
    JSON.stringify(firstRun) === JSON.stringify([["true", "nobody"], ["id -u", "nobody"]]) &&
      asUser.exitCode === 0,
    JSON.stringify(firstRun),
  );
  seen.execBodies = [];
  await m.exec(["id", "-u"], { user: "nobody" });
  check(
    "a proven user is not probed again on the same handle",
    seen.execBodies.length === 1 && seen.execBodies[0].command[0] === "id",
    `${seen.execBodies.length} request(s)`,
  );
  // A control plane that predates per-command users drops the field: the SDK
  // must refuse BEFORE the real command runs as the default account.
  echoUser = false;
  const old = await Machine.connect("m1", cloudConn);
  seen.execBodies = [];
  let oldErr: unknown;
  try {
    await old.exec(["rm", "-rf", "/data"], { user: "app" });
  } catch (e) {
    oldErr = e;
  }
  check(
    "an old control plane is refused with NotSupportedError",
    oldErr instanceof NotSupportedError,
    String(oldErr),
  );
  check(
    "...and the real command never reaches it",
    seen.execBodies.length === 1 && seen.execBodies[0].command[0] === "true",
    JSON.stringify(seen.execBodies.map((b: any) => b.command)),
  );
  let streamErr: unknown;
  try {
    for await (const _ of old.execStream(["id"], { user: "app" })) {
      /* unreachable */
    }
  } catch (e) {
    streamErr = e;
  }
  check("execStream is refused the same way", streamErr instanceof NotSupportedError, String(streamErr));
  echoUser = true;

  await m.stop();
  await m.delete();
  check("stop + delete over REST (no throw)", true);

  // Cache disks: the account API, a machine started from one, and publishing.
  const cloud = { target: "cloud" as const, baseUrl, apiKey: "smk_test123" };
  const created = await CacheDisk.create({ name: "deps", sizeGb: 10 }, cloud);
  check(
    "CacheDisk.create posts name and size",
    JSON.stringify(seen.cacheDiskCreate) === JSON.stringify({ name: "deps", sizeGb: 10 }) && created.latestVersion === 0,
    JSON.stringify(seen.cacheDiskCreate),
  );
  const listed = await CacheDisk.list(cloud);
  check("CacheDisk.list unwraps cacheDisks", listed.length === 1 && listed[0].name === "deps");
  await CacheDisk.get("deps", cloud);
  check("CacheDisk.get addresses by name", seen.cacheDiskGet === "deps", String(seen.cacheDiskGet));
  const withCache = await Machine.create(
    { image: "alpine", cacheDisk: { cache: "deps", version: 0, mountPath: "/deps" } },
    cloud,
  );
  check(
    "cloud create sends cacheDisk",
    JSON.stringify(seen.createBody?.cacheDisk) === JSON.stringify({ cache: "deps", version: 0, mountPath: "/deps" }),
    JSON.stringify(seen.createBody?.cacheDisk),
  );
  const published = await withCache.publishCacheDisk();
  check(
    "publishCacheDisk posts to the machine and returns the new version",
    String(seen.cacheDiskPublish).endsWith("/cache-disk/publish") && published.version.version === 1,
    String(seen.cacheDiskPublish),
  );
  await CacheDisk.delete("deps", cloud);
  check("CacheDisk.delete addresses by name", seen.cacheDiskDelete === "deps", String(seen.cacheDiskDelete));
  let localCacheRefused = false;
  try {
    await Machine.create({ image: "alpine", cacheDisk: { cache: "deps" } }, { target: "local" });
  } catch (e) {
    localCacheRefused = e instanceof NotSupportedError;
  }
  check("a local cacheDisk is refused as NotSupported", localCacheRefused);

  console.log(`\n${passed} passed, ${failed} failed`);
  server.close();
  if (failed > 0) process.exit(1);
}

main().catch((e) => {
  console.error("cloud-mock crashed:", e);
  server.close();
  process.exit(1);
});
