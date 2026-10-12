/**
 * The eve sandbox provider on real local machines, driven the way eve's
 * runtime drives it: prepare once, start sessions with their own network
 * policies, stop and resume, delete. Needs Node 24 with eve installed, KVM (or
 * Apple Silicon) and outbound HTTPS.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Machine } from "../index";
import { SmolmachinesSandbox, toMachineNetworkPolicy } from "../eve";

let failures = 0;
function check(label: string, ok: boolean, detail = "") {
  if (!ok) failures++;
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  (${detail})` : ""}`);
}
async function rejects(p: PromiseLike<unknown>): Promise<string> {
  try {
    await p;
    return "";
  } catch (e) {
    return (e as Error).message;
  }
}

type Impl = {
  prepare(ctx: unknown): Promise<any>;
  start(ctx: unknown, open: unknown, artifact: any): Promise<{ handle: any; state: any }>;
  resume(ctx: unknown, artifact: any, state: any): Promise<any>;
  onSessionEnd(ctx: unknown, artifact: any, state: any, options: { reason: "completed" | "expired" | "failed" }): Promise<void>;
};
/** The implementation eve's runtime calls, as recorded on the environment. */
const implementationOf = (environment: object): Impl =>
  (environment as Record<symbol, { implementation: Impl }>)[Symbol.for("eve.sandbox-provider-runtime")]!.implementation;

async function reach(sandbox: any, url: string): Promise<string> {
  const r = await sandbox.run({ command: `curl -s -o /dev/null -m 8 -w '%{http_code}' ${url} || echo blocked` });
  return r.stdout.trim().replace(/^000/, "");
}

async function main() {
  const storagePath = mkdtempSync(join(tmpdir(), "eve-smol-"));
  const log: string[] = [];
  const resources = {
    source: { kind: "inline", key: "r1" },
    workspace: {
      key: "r1:workspace",
      mountPath: "/eve/resources/workspace",
      targetPath: "/workspace",
      files: [{ relativePath: "data/schema.sql", content: "create table t (id int);\n" }],
    },
    skills: {
      key: "r1:skills",
      mountPath: "/eve/resources/skills",
      targetPath: "$HOME/.agents/skills",
      files: [{ relativePath: "greet/SKILL.md", content: new TextEncoder().encode("# greet\n") }],
    },
  };
  const prepareCtx = {
    files: { list: async () => [], read: async () => new Uint8Array(), readText: async () => "" },
    host: {},
    log: (m: string) => log.push(m),
    resources,
    sourceRevision: "rev-1",
    storagePath,
  };
  const sessionCtx = (id: string) => ({ host: {}, session: { id }, storagePath });

  let prepareRuns = 0;
  const environment = SmolmachinesSandbox.environment({
    env: { TEAM: "smol" },
    prepare: async (sandbox) => {
      prepareRuns++;
      const r = await sandbox.run({ command: "echo prepared > .prepared && command -v curl" });
      if (r.exitCode !== 0) throw new Error(`prepare: ${r.stderr}`);
    },
  });
  const impl = implementationOf(environment);
  const handles: any[] = [];
  try {
    let t0 = Date.now();
    const artifact = await impl.prepare(prepareCtx);
    check("prepare builds a checkpoint from eve's default image", /ghcr\.io\/vercel\/eve:/.test(artifact.image) && prepareRuns === 1, `${artifact.image}, ${Date.now() - t0} ms`);
    t0 = Date.now();
    const again = await impl.prepare(prepareCtx);
    check("an unchanged environment reuses its checkpoint", again.checkpoint === artifact.checkpoint && prepareRuns === 1, `${Date.now() - t0} ms`);

    t0 = Date.now();
    const scoped = await impl.start(sessionCtx("session-a"), { networkPolicy: { allow: ["example.com"] } }, artifact);
    handles.push(scoped.handle);
    const sb = scoped.handle.sandbox;
    check("a session starts from the prepared checkpoint", (await sb.readTextFile({ path: ".prepared" })) === "prepared\n", `${Date.now() - t0} ms`);
    const who = await sb.run({ command: 'echo "$(id -un) $HOME $(pwd) $TEAM"' });
    check("commands run as the sandbox user in /workspace with the environment's env", who.stdout.trim() === "vercel-sandbox /home/vercel-sandbox /workspace smol", who.stdout.trim());
    const fresh = await sb.run({ command: "cat /proc/sys/vm/dirty_expire_centisecs" });
    check("a started machine flushes writes within a second", fresh.stdout === "100\n", JSON.stringify(fresh.stdout));
    check("workspace seeds land in /workspace", (await sb.readTextFile({ path: "data/schema.sql" }))?.startsWith("create table") === true);
    const skill = await sb.run({ command: 'cat "$HOME/.agents/skills/greet/SKILL.md" && stat -c %U "$HOME/.agents/skills/greet/SKILL.md" data/schema.sql' });
    check("skills land in $HOME/.agents/skills, all owned by the sandbox user", skill.stdout === "# greet\nvercel-sandbox\nvercel-sandbox\n", JSON.stringify(skill.stdout));
    check("the session's allow list admits its host", (await reach(sb, "https://example.com")) === "200");
    check("and refuses everything else", (await reach(sb, "https://example.org")) === "blocked");

    const denied = await impl.start(sessionCtx("session-b"), { networkPolicy: "deny-all" }, artifact);
    handles.push(denied.handle);
    check("a deny-all session reaches nothing", (await reach(denied.handle.sandbox, "https://example.com")) === "blocked");
    check("sessions get their own machines", scoped.state.machine !== denied.state.machine);

    await sb.writeTextFile({ path: "notes.md", content: "kept across stop\n" });
    check("older Eve stop and delete hooks remain available", typeof scoped.handle.onSessionStop === "function" && typeof scoped.handle.onSessionDelete === "function");
    await scoped.handle.onSandboxStop();
    check("stopping a session stops its machine", (await rejects(sb.run({ command: "true" }))) !== "");
    const resumed = await impl.resume(sessionCtx("session-a"), artifact, scoped.state);
    handles.push(resumed);
    check("a stopped session resumes with its files", (await resumed.sandbox.readTextFile({ path: "notes.md" })) === "kept across stop\n");
    check("and keeps its network policy", (await reach(resumed.sandbox, "https://example.org")) === "blocked");
    const writeback = await resumed.sandbox.run({ command: "cat /proc/sys/vm/dirty_expire_centisecs /proc/sys/vm/dirty_writeback_centisecs" });
    check("a resumed machine flushes writes within a second", writeback.stdout === "100\n100\n", JSON.stringify(writeback.stdout));
    await resumed.onSandboxStop();
    await impl.onSessionEnd(sessionCtx("session-a"), artifact, scoped.state, { reason: "completed" });
    check("ending a durable session reclaims its stopped machine", (await rejects(Machine.connect(scoped.state.machine, { target: "local", handleSignals: false }))) !== "");

    await denied.handle.onSandboxDelete();
    check("deleting a session deletes its machine", (await rejects(Machine.connect(denied.state.machine, { target: "local", handleSignals: false }))) !== "");
    check("a deleted session cannot resume", /no longer available/.test(await rejects(impl.resume(sessionCtx("session-b"), artifact, denied.state))));

    // Any image works: the provider creates the sandbox user when the image lacks it.
    const alpine = implementationOf(SmolmachinesSandbox.image("alpine"));
    const alpineArtifact = await alpine.prepare({ ...prepareCtx, sourceRevision: "rev-alpine" });
    const onAlpine = await alpine.start(sessionCtx("session-alpine"), undefined, alpineArtifact);
    handles.push(onAlpine.handle);
    const alpineWho = await onAlpine.handle.sandbox.run({ command: 'echo "$(id -un) $HOME $(pwd)"; cat data/schema.sql' });
    check("an image without the sandbox user gets one", alpineWho.stdout === "vercel-sandbox /home/vercel-sandbox /workspace\ncreate table t (id int);\n", JSON.stringify(alpineWho.stdout));

    const rules = await rejects(impl.start(sessionCtx("session-c"), { networkPolicy: { allow: { "api.github.com": [{ transform: [{ headers: { a: "b" } }] }] } } }, artifact));
    check("request rules are refused before a machine starts", /request rules/.test(rules), rules.slice(0, 80));
  } finally {
    for (const h of handles) await h.onSandboxDelete().catch(() => {});
    rmSync(storagePath, { recursive: true, force: true });
  }

  check("policy mapping: '*' allows all", toMachineNetworkPolicy({ allow: ["*"] }) === "allow-all");
  const mapped = toMachineNetworkPolicy({ allow: { "*.npmjs.org": [] }, subnets: { allow: ["10.0.0.0/8"] } });
  check("policy mapping: record hosts and subnets", JSON.stringify(mapped) === JSON.stringify({ allowHosts: ["*.npmjs.org"], allowCidrs: ["10.0.0.0/8"] }), JSON.stringify(mapped));
  check("policy mapping: subnets.deny is refused", /subnets\.deny/.test(await rejects(Promise.resolve().then(() => toMachineNetworkPolicy({ subnets: { deny: ["10.0.0.0/8"] } })))));

  console.log(`\nprovider log: ${log.join(" | ")}`);
  console.log(`${failures === 0 ? "ALL PASSED" : `${failures} FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("EVE E2E CRASHED:", e);
  process.exit(2);
});
