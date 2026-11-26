import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Machine } from '../index';

function databasePath(root: string): string {
  return process.platform === 'win32'
    ? join(root, 'AppData', 'Local', 'smolvm', 'server', 'smolvm.db')
    : process.platform === 'darwin'
      ? join(root, 'Library', 'Application Support', 'smolvm', 'server', 'smolvm.db')
      : join(root, '.local', 'share', 'smolvm', 'server', 'smolvm.db');
}

async function main(): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), 'smol-data-dir-'));
  const otherRoot = mkdtempSync(join(tmpdir(), 'smol-data-dir-other-'));
  let machine: Machine | undefined;
  try {
    machine = await Machine.create({ dataDir: root });
    assert.ok(
      existsSync(databasePath(root)),
      `local machine database was not created at ${databasePath(root)}`,
    );
    await machine.delete();
    machine = undefined;
    await assert.rejects(
      Machine.create({ dataDir: otherRoot }),
      { code: 'INVALID_CONFIG' },
      'changing the embedded engine data root must be rejected',
    );
    console.log(`dataDir isolated the local machine database at ${databasePath(root)}`);
  } finally {
    await machine?.delete();
    if (process.platform !== 'win32') {
      rmSync(root, { recursive: true, force: true });
      rmSync(otherRoot, { recursive: true, force: true });
    }
  }
}

main().catch((error: unknown) => { console.error(error); process.exitCode = 1; });
