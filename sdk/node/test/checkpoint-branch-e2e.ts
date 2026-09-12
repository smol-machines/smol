/** Real local portable-checkpoint and native batch-branch smoke test. */

import { mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Machine } from "../index";

async function main(): Promise<void> {
  const suffix = `${process.pid}-${Date.now()}`;
  const root = mkdtempSync(join(tmpdir(), `smol-sdk-${suffix}-`));
  const store = join(root, "store");
  const firstArtifact = join(root, "checkpoint-1.smolcheckpoint");
  const secondArtifact = join(root, "checkpoint-2.smolcheckpoint");
  const portableArtifact = join(root, "checkpoint-2-portable.smolcheckpoint");
  let source: Machine | undefined;
  let restored: Machine | undefined;
  const children: Machine[] = [];

  try {
    source = await Machine.create({
      name: `sdk-checkpoint-source-${suffix}`,
      image: "nginx:1.27-alpine",
      resources: { cpus: 2, memoryMb: 1024, network: true },
      persistent: true,
      branchable: true,
    });
    await source.writeFile("/dev/shm/ram-marker", "RAM-STATE");
    await source.writeFile("/tmp/disk-marker", "DISK-STATE");

    const first = await source.checkpoint(firstArtifact, { store });
    if (first.path !== firstArtifact || first.sizeBytes <= 0) {
      throw new Error("local checkpoint metadata was incomplete");
    }
    await source.writeFile("/dev/shm/ram-marker", "RAM-STATE-2");
    await source.writeFile("/tmp/disk-marker", "DISK-STATE-2");
    const second = await source.checkpoint(secondArtifact, { store });
    if (!second.reusedBytes || second.reusedBytes <= 0) {
      throw new Error("periodic checkpoint did not reuse existing objects");
    }
    rmSync(firstArtifact, { recursive: true });
    const reclaimed = Machine.pruneCheckpointStore(store);
    if (reclaimed <= 0) throw new Error("checkpoint prune reclaimed no objects");
    const exportedBytes = Machine.exportCheckpoint(secondArtifact, portableArtifact);
    if (exportedBytes <= 0 || statSync(portableArtifact).size <= 0) {
      throw new Error("stored checkpoint export was empty");
    }
    const sourceAlive = await source.exec([
      "sh",
      "-c",
      'test "$(cat /dev/shm/ram-marker)" = RAM-STATE-2 && echo SOURCE-ALIVE',
    ]);
    if (sourceAlive.stdout.trim() !== "SOURCE-ALIVE") {
      throw new Error("source did not continue after checkpoint capture");
    }

    restored = await Machine.restoreCheckpoint(secondArtifact, `sdk-restored-${suffix}`);
    const ram = (await restored.readFile("/dev/shm/ram-marker")).toString();
    const disk = (await restored.readFile("/tmp/disk-marker")).toString();
    if (ram !== "RAM-STATE-2" || disk !== "DISK-STATE-2") {
      throw new Error(`restored state mismatch: ram=${ram} disk=${disk}`);
    }

    children.push(
      ...(await restored.branchBatch({
        names: [`sdk-child-a-${suffix}`, `sdk-child-b-${suffix}`],
      })),
    );
    if (children.length !== 2) throw new Error("native batch returned the wrong size");
    await children[0].writeFile("/tmp/child-only", "A");
    const sibling = await children[1].exec([
      "sh",
      "-c",
      "test ! -e /tmp/child-only && echo ISOLATED",
    ]);
    if (sibling.stdout.trim() !== "ISOLATED") {
      throw new Error("batch branch sibling isolation failed");
    }

    console.log("checkpoint-branch-e2e: passed");
  } finally {
    await Promise.allSettled(children.map((machine) => machine.delete()));
    if (restored) await restored.delete().catch(() => {});
    if (source) await source.delete().catch(() => {});
    rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
