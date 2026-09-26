import { strict as assert } from "node:assert";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { AgentSession } from "../index";

const calls: { method: string; path: string; body: string; key?: string }[] = [];
let legacyOnly = false;
const info = { name: "fixer", harness: "claude-code", status: "ready", machineId: "mach-1", turns: [], createdAt: "2026-01-01T00:00:00Z" };
const turn = { index: 0, prompt: "fix tests", status: "done", isError: false, checkpointed: true, startedAt: "2026-01-01T00:00:00Z" };
const server = createServer(async (request, response) => {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(chunk);
  const path = request.url ?? "";
  calls.push({ method: request.method ?? "", path, body: Buffer.concat(chunks).toString(), key: request.headers["idempotency-key"] as string | undefined });
  const json = (body: unknown) => { response.setHeader("content-type", "application/json"); response.end(JSON.stringify(body)); };
  if (path.endsWith("/events")) {
    response.setHeader("content-type", "text/event-stream");
    response.write('event: event\r');
    response.write('\nid: 7\r\ndata: {"type":"text"}\r\n\r\n');
    response.end(`event: done\ndata: ${JSON.stringify(turn)}\n\n`);
  } else if (path === "/v1/machines/mach-1") json({ id: "mach-1", name: "fixer-vm" });
  else if (path === "/v1/agents/fixer/turns") json({ turn: 0 });
  else if (path === "/v1/agents/fixer/branch" && legacyOnly) { response.statusCode = 404; json({ code: "NOT_FOUND", error: "route not found" }); }
  else if (path === "/v1/agents/fixer/branch" || path === "/v1/agents/fixer/fork") json({ ...info, name: "alternative" });
  else if (path === "/v1/agents") json(request.method === "GET" ? { items: [info] } : info);
  else if (request.method === "DELETE" || path.endsWith("/cancel") || path.endsWith("/pause") || path.endsWith("/resume")) { response.statusCode = 204; response.end(); }
  else json(info);
});

server.listen(0, "127.0.0.1", async () => {
  try {
    const baseUrl = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
    const conn = { target: "cloud" as const, baseUrl, apiKey: "smk_test" };
    const session = await AgentSession.create({ name: "fixer", harness: "claude-code", credential: "anthropic" }, conn);
    assert.equal((await session.info()).status, "ready");
    assert.equal((await session.machine()).id, "mach-1");
    assert.equal((await AgentSession.list(conn)).items[0].name, "fixer");
    assert.equal(await session.send("fix tests", { idempotencyKey: "task-1" }), 0);
    const events = [];
    for await (const event of session.events(0)) events.push(event);
    assert.deepEqual(events.map((event) => event.type), ["event", "done"]);
    assert.equal(events[0].type === "event" && events[0].id, 7);
    assert.equal((await session.branch(0, "alternative")).name, "alternative");
    assert.equal((await session.fork(0, "alternative")).name, "alternative");
    assert.equal(calls.filter((call) => call.path === "/v1/agents/fixer/branch").length, 2);
    legacyOnly = true;
    assert.equal((await session.branch(0, "alternative")).name, "alternative");
    assert.equal(calls.filter((call) => call.path === "/v1/agents/fixer/fork").length, 1);
    await session.cancel(0);
    await session.pause();
    await session.resume();
    await session.delete();
    assert.equal(calls.find((call) => call.path === "/v1/agents/fixer/turns")?.key, "task-1");
    assert.equal(JSON.parse(calls[0].body).credential, "anthropic");
    assert(calls.every((call) => call.path.startsWith("/v1/agents") || call.path === "/v1/machines/mach-1"));
    console.log("managed agent SDK cloud contract passed");
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  } finally {
    server.close();
  }
});
