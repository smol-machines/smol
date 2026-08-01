/** Verify Bun can load the built package and start a bundled local machine. */

import assert from 'node:assert/strict';
import { Machine } from 'smolmachines';

async function main(): Promise<void> {
  const machine = await Machine.create({
    resources: { cpus: 2, memoryMb: 1024 },
  });

  try {
    const result = await machine.exec(['printf', 'hello-from-bun']);
    assert.equal(result.exitCode, 0);
    assert.equal(result.stdout, 'hello-from-bun');
  } finally {
    await machine.delete();
  }

  console.log('Bun package lifecycle passed');
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
