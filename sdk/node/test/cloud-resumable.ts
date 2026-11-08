/**
 * Resumable execs on the cloud target, against a localhost mock control plane
 * that cuts off connections mid-command the way a restarting control does.
 *
 *   npx tsx test/cloud-resumable.ts
 */

import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { Machine } from "../index";
import { resumableVerdict } from "../transport";

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

const LINES = 30;
const output = Buffer.from(Array.from({ length: LINES }, (_, i) => `line ${i}\n`).join(""));
const seen = { requests: 0, starts: [] as any[], deletes: 0, plainExecs: 0, fileGets: 0 };
let refuseFirstStart = true;

function readBody(req: any): Promise<string> {
  return new Promise((resolve) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks).toString()));
  });
}

const server = createServer(async (req, res) => {
  const url = req.url ?? "";
  const method = req.method ?? "GET";
  const json = (code: number, obj: unknown) => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(obj));
  };
  if (method === "GET" && /^\/v1\/machines\/m-(new|old)$/.test(url)) {
    return json(200, { id: url.split("/").pop(), state: "started", ready: true });
  }
  if (url.startsWith("/v1/machines/m-old/execs")) return json(404, { code: "not_found", detail: "no such route" });
  if (url.startsWith("/v1/machines/m-new/files/") && url.endsWith("data.bin")) {
    seen.fileGets++;
    // The first download is cut off midway, as a control restarting does.
    if (seen.fileGets === 1) {
      res.writeHead(200, { "content-type": "application/octet-stream", "content-length": "1000" });
      res.write(Buffer.alloc(100, 7));
      setTimeout(() => req.socket.destroy(), 20);
      return;
    }
    res.writeHead(200, { "content-type": "application/octet-stream" });
    return res.end(Buffer.alloc(1000, 7));
  }
  if (method === "POST" && url === "/v1/machines/m-old/exec") {
    seen.plainExecs++;
    return json(200, { exitCode: 0, stdout: "plain\n", stderr: "" });
  }
  if (!url.startsWith("/v1/machines/m-new/execs")) return json(404, { code: "not_found" });
  seen.requests++;
  if (method === "POST") {
    seen.starts.push(JSON.parse(await readBody(req)));
    if (refuseFirstStart && seen.starts.length === 1) {
      return json(503, { code: "control_draining", retryable: true });
    }
  }
  if (method === "DELETE") {
    seen.deletes++;
    res.writeHead(204);
    return res.end();
  }
  // Every third read is cut off without an answer, as a control dying is.
  if (method === "GET" && seen.requests % 3 === 0) {
    req.socket.destroy();
    return;
  }
  const after = Number(new URL(url, "http://x").searchParams.get("after") ?? 0);
  const end = Math.min(after + 7, output.length);
  const done = end === output.length;
  return json(method === "POST" ? 201 : 200, {
    execId: "x",
    state: done ? "exited" : "running",
    done,
    ...(done ? { exitCode: 4 } : {}),
    output: [{ stream: after % 2 === 0 ? "stdout" : "stderr", offset: after, data: output.subarray(after, end).toString("base64") }],
    next: end,
    start: after,
    end: output.length,
  });
});

async function main() {
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
  const baseUrl = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  const cloud = { target: "cloud" as const, baseUrl, apiKey: "smk_test123" };

  const m = await Machine.connect("m-new", cloud);
  const r = await m.exec(["build"]);
  const bytes = Buffer.concat([Buffer.from(r.stdoutBytes ?? []), Buffer.from(r.stderrBytes ?? [])]);
  const sorted = (b: Buffer) => [...b].sort((a, c) => a - c).join(",");
  check(
    "an exec rides out cut-off connections: every byte once, and the exit code",
    sorted(bytes) === sorted(output) && r.exitCode === 4,
    `got ${bytes.length}/${output.length} bytes, exit ${r.exitCode}`,
  );
  const ids = new Set(seen.starts.map((s) => s.execId));
  check(
    "a start refused while the control drains is retried under the same exec id",
    seen.starts.length === 2 && ids.size === 1 && seen.starts[0].waitSeconds === 20,
    JSON.stringify(seen.starts),
  );
  check("the collected exec is deleted", seen.deletes === 1, `deletes=${seen.deletes}`);

  refuseFirstStart = false;
  seen.requests = 0;
  let text = "";
  let exit: number | undefined;
  for await (const ev of m.execStream(["build"])) {
    if (ev.kind === "stdout" || ev.kind === "stderr") text += ev.data;
    if (ev.kind === "exit") exit = ev.exitCode;
    if (ev.kind === "error") throw new Error(ev.message);
  }
  check("a streamed exec delivers its output in order across cut-offs", text === output.toString() && exit === 4, JSON.stringify(text.slice(0, 40)));

  const file = await m.readFile("/data.bin");
  check("a download cut off midway is read again whole", file.length === 1000 && seen.fileGets === 2, `len=${file.length} gets=${seen.fileGets}`);

  const old = await Machine.connect("m-old", cloud);
  const plain = await old.exec(["true"]);
  check("a control plane without resumable execs gets the plain exec", plain.stdout === "plain\n" && seen.plainExecs === 1);

  check(
    "a start is retried only where it cannot have reached a node",
    resumableVerdict(503, "control_draining", true) === "retry" &&
      resumableVerdict(503, "unavailable", true) === "fail" &&
      resumableVerdict(503, "unavailable", false) === "retry" &&
      resumableVerdict(503, "machine_interrupted", false) === "fail" &&
      resumableVerdict(404, undefined, true) === "unsupported" &&
      resumableVerdict(404, "exec_not_found", false) === "fail",
  );

  server.close();
  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) process.exit(1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
