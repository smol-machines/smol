/** Local staged-mount E2E: explicit sync and graceful-stop sync. */

import assert from "node:assert";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Machine } from "../index";

async function main(): Promise<void> {
  const host = mkdtempSync(join(tmpdir(), "smol-staged-node-"));
  const name = `staged-node-${process.pid}-${Date.now()}`;
  writeFileSync(join(host, "state.txt"), "initial\n");
  const machine = await Machine.create({
    name,
    image: "alpine:3.20",
    persistent: true,
    mounts: [{ source: host, target: "/work", staged: true }],
    resources: { cpus: 1, memoryMb: 512, network: true },
  });
  try {
    (await machine.exec(["sh", "-c", "echo explicit > /work/state.txt"]))
      .assertSuccess();
    assert.strictEqual(readFileSync(join(host, "state.txt"), "utf8"), "initial\n");
    await machine.sync();
    assert.strictEqual(readFileSync(join(host, "state.txt"), "utf8"), "explicit\n");

    (await machine.exec(["sh", "-c", "echo stopped > /work/state.txt"]))
      .assertSuccess();
    await machine.stop();
    assert.strictEqual(readFileSync(join(host, "state.txt"), "utf8"), "stopped\n");
    console.log("staged mount explicit sync + graceful stop: PASS");
  } finally {
    await machine.delete();
    rmSync(host, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
