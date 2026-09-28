/**
 * One prepared checkpoint, many sessions, each with its own network policy:
 * a machine is prepared once on virtio-net and checkpointed, then restored
 * several times with an allow list, deny-all and allow-all applied before each
 * first boot. Needs KVM (or Apple Silicon) and outbound HTTPS.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Machine, type NetworkPolicy } from "../index";

const opts = { target: "local" as const, handleSignals: false };
let failures = 0;
function check(label: string, ok: boolean, detail = "") {
  if (!ok) failures++;
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  (${detail})` : ""}`);
}

/** HTTP status the guest gets from `url`, or "blocked" when it can't connect. */
async function reach(m: Machine, url: string): Promise<string> {
  const r = await m.exec(["sh", "-c", `curl -s -o /dev/null -m 8 -w '%{http_code}' ${url} || echo blocked`]);
  return r.stdout.trim().replace(/^000/, "");
}

async function main() {
  const pid = process.pid;
  const dir = mkdtempSync(join(tmpdir(), "netpol-"));
  const prepared = join(dir, "prepared.smolcheckpoint");
  const sessions: Machine[] = [];
  try {
    // Prepare once, with open egress (the install needs it), on virtio-net.
    const prep = await Machine.create(
      { name: `netpol-prep-${pid}`, image: "alpine", branchable: true, resources: { network: true, networkBackend: "virtio-net" } },
      opts,
    );
    try {
      const install = await prep.exec(["sh", "-c", "apk add -q curl && echo prepared > /root/marker"]);
      check("the environment is prepared with open egress", install.exitCode === 0, install.stderr.trim().slice(0, 80));
      await prep.checkpoint(prepared);
    } finally {
      await prep.delete().catch(() => {});
    }

    const restore = async (label: string, networkPolicy?: NetworkPolicy) => {
      const t0 = Date.now();
      const m = await Machine.restoreCheckpoint(prepared, `netpol-${label}-${pid}`, opts, networkPolicy === undefined ? undefined : { networkPolicy });
      sessions.push(m);
      return { m, ms: Date.now() - t0 };
    };

    const { m: scoped, ms } = await restore("scoped", { allowHosts: ["example.com"] });
    check("a session restores from the prepared checkpoint", (await scoped.exec(["cat", "/root/marker"])).stdout.trim() === "prepared", `${ms} ms`);
    check("its allow list admits the named host", (await reach(scoped, "https://example.com")) === "200");
    check("and refuses everything else", (await reach(scoped, "https://example.org")) === "blocked", await reach(scoped, "https://example.org"));

    const { m: denied } = await restore("denied", "deny-all");
    check("a deny-all session reaches nothing", (await reach(denied, "https://example.com")) === "blocked", await reach(denied, "https://example.com"));
    check("but keeps the prepared environment", (await denied.exec(["cat", "/root/marker"])).stdout.trim() === "prepared");

    const { m: open } = await restore("open", "allow-all");
    check("an allow-all session reaches any host", (await reach(open, "https://example.org")) === "200", await reach(open, "https://example.org"));

    const { m: inherited } = await restore("inherited");
    check("without a policy a session keeps the prepared machine's (open)", (await reach(inherited, "https://example.org")) === "200");

    // A stopped session can have its policy replaced again.
    await scoped.stop();
    await scoped.setNetworkPolicy({ allowHosts: ["*.example.com"] });
    await scoped.start();
    const sub = await reach(scoped, "https://www.example.com");
    const apex = await reach(scoped, "https://example.com");
    check("a stopped session's policy can be replaced (subdomains only)", sub === "200" && apex === "blocked", `www ${sub}, apex ${apex}`);

    let running = "";
    try {
      await open.setNetworkPolicy("deny-all");
    } catch (e) {
      running = (e as Error).message;
    }
    check("a running machine's policy is not changed underneath it", /stopped/i.test(running), running.slice(0, 100));
  } finally {
    for (const m of sessions) await m.delete().catch(() => {});
    rmSync(dir, { recursive: true, force: true });
  }

  // A checkpoint taken on TSI cannot take an allow list: its guest has no
  // virtio-net device.
  const dir2 = mkdtempSync(join(tmpdir(), "netpol-tsi-"));
  const tsiCheckpoint = join(dir2, "tsi.smolcheckpoint");
  const tsi = await Machine.create({ name: `netpol-tsi-${pid}`, image: "alpine", branchable: true, network: true }, opts);
  try {
    await tsi.exec(["true"]);
    await tsi.checkpoint(tsiCheckpoint);
  } finally {
    await tsi.delete().catch(() => {});
  }
  let refused = "";
  try {
    const m = await Machine.restoreCheckpoint(tsiCheckpoint, `netpol-tsi-restored-${pid}`, opts, { networkPolicy: { allowHosts: ["example.com"] } });
    await m.delete().catch(() => {});
  } catch (e) {
    refused = (e as Error).message;
  }
  check("an allow list on a TSI checkpoint is refused, not silently unenforced", /cannot enforce an allow list/.test(refused), refused.slice(0, 120));
  rmSync(dir2, { recursive: true, force: true });

  console.log(`\n${failures === 0 ? "ALL PASSED" : `${failures} FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("NETWORK POLICY E2E CRASHED:", e);
  process.exit(2);
});
