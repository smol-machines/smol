import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Machine } from '../index';

async function main(): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), 'smol-data-dir-'));
  const previous = process.env.SMOLVM_DATA_DIR;
  process.env.SMOLVM_DATA_DIR = root;
  try {
    assert.deepEqual(await Machine.list({ target: 'local' }), []);
    const database = process.platform === 'win32'
      ? join(root, 'AppData', 'Local', 'smolvm', 'server', 'smolvm.db')
      : process.platform === 'darwin'
        ? join(root, 'Library', 'Application Support', 'smolvm', 'server', 'smolvm.db')
        : join(root, '.local', 'share', 'smolvm', 'server', 'smolvm.db');
    assert.ok(existsSync(database), `local machine database was not created at ${database}`);
    console.log(`SMOLVM_DATA_DIR isolated the local machine database at ${database}`);
  } finally {
    if (previous === undefined) delete process.env.SMOLVM_DATA_DIR;
    else process.env.SMOLVM_DATA_DIR = previous;
    if (process.platform !== 'win32') rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error: unknown) => { console.error(error); process.exitCode = 1; });
