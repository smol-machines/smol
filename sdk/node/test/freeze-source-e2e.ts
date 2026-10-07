/** Real local test: a branch can leave its source frozen as a reusable base. */

import { Machine } from "../index";

async function main(): Promise<void> {
  const suffix = `${process.pid}-${Date.now()}`;
  let source: Machine | undefined;
  const children: Machine[] = [];

  try {
    source = await Machine.create({
      name: `sdk-freeze-source-${suffix}`,
      image: "alpine:3.20",
      resources: { cpus: 1, memoryMb: 512, network: true },
      persistent: true,
      branchable: true,
    });
    await source.writeFile("/dev/shm/ram-marker", "FROZEN-STATE");

    children.push(await source.branch(`sdk-freeze-a-${suffix}`, { freezeSource: true }));
    const first = await children[0].exec(["cat", "/dev/shm/ram-marker"]);
    if (first.stdout.trim() !== "FROZEN-STATE") {
      throw new Error(`branch did not inherit the source's RAM: ${first.stdout}`);
    }

    // A frozen source is a branch base; it does not run commands.
    let ran = false;
    try {
      await source.exec(["true"]);
      ran = true;
    } catch {
      // expected
    }
    if (ran) throw new Error("a frozen source still ran a command");

    // Later branches start from the same frozen state, one at a time or in a batch.
    children.push(await source.branch(`sdk-freeze-b-${suffix}`, { freezeSource: true }));
    children.push(
      ...(await source.branchBatch({
        names: [`sdk-freeze-c-${suffix}`, `sdk-freeze-d-${suffix}`],
        freezeSource: true,
      })),
    );
    for (const child of children) {
      const seen = await child.exec(["cat", "/dev/shm/ram-marker"]);
      if (seen.stdout.trim() !== "FROZEN-STATE") {
        throw new Error(`a later branch did not start from the frozen state: ${seen.stdout}`);
      }
    }
    console.log(`ok: ${children.length} branches from one frozen source`);
  } finally {
    for (const child of children.reverse()) await child.delete().catch(() => {});
    await source?.delete().catch(() => {});
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
