/** Pure-unit tests — no VM boot, no network. Covers the error-parsing seam and
 *  cloud path encoding, the two places most likely to silently regress. */
import assert from 'node:assert';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { wrapNativeError, SmolError } from '../errors';
import { cliConfigApiKey, encodePath, toNativeConfig } from '../transport';

let passed = 0;
let failed = 0;
function check(name: string, fn: () => void) {
  try {
    fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (e) {
    failed++;
    console.log(`  ✗ ${name}: ${(e as Error).message}`);
  }
}

console.log('smol SDK unit tests\n');

// --- wrapNativeError: native `[CODE] message` → typed SmolError ---
check('parses "[CODE] message" into code + message', () => {
  const e = wrapNativeError(new Error('[KVM_UNAVAILABLE] /dev/kvm missing'));
  assert.strictEqual(e.code, 'KVM_UNAVAILABLE');
  assert.strictEqual(e.message, '/dev/kvm missing');
});
check('unbracketed message falls back to SMOLVM_ERROR', () => {
  const e = wrapNativeError(new Error('boom'));
  assert.strictEqual(e.code, 'SMOLVM_ERROR');
  assert.strictEqual(e.message, 'boom');
});
check('multiline message after [CODE] is preserved (dotAll)', () => {
  const e = wrapNativeError(new Error('[X] line1\nline2'));
  assert.strictEqual(e.code, 'X');
  assert.strictEqual(e.message, 'line1\nline2');
});
check('an existing SmolError is passed through unchanged', () => {
  const orig = new SmolError('CUSTOM', 'already typed');
  assert.strictEqual(wrapNativeError(orig), orig);
});
check('non-Error input is coerced to a string message', () => {
  const e = wrapNativeError({ weird: true });
  assert.strictEqual(e.code, 'SMOLVM_ERROR');
  assert.ok(e instanceof SmolError);
});

// --- encodePath: keep `/` (wildcard route), escape unsafe chars ---
check('keeps path separators', () => {
  assert.strictEqual(encodePath('/tmp/a/b.txt'), '/tmp/a/b.txt');
});
check('escapes spaces', () => {
  assert.strictEqual(encodePath('/tmp/my file.txt'), '/tmp/my%20file.txt');
});
check('escapes ? and # (would otherwise truncate the URL)', () => {
  assert.strictEqual(encodePath('/a/b?c#d'), '/a/b%3Fc%23d');
});
check('escapes % so double-encoding is unambiguous', () => {
  assert.strictEqual(encodePath('/a/100%done'), '/a/100%25done');
});

// --- toNativeConfig: GPU resources map to the native (snake→camel) field ---
check('forwards gpu + gpuVramMib to native resources', () => {
  const cfg = toNativeConfig('m', { resources: { gpu: true, gpuVramMib: 512 } });
  assert.strictEqual(cfg.resources?.gpu, true);
  assert.strictEqual(cfg.resources?.gpuVramMib, 512);
});
check('omits gpu fields when unset (engine defaults apply)', () => {
  const cfg = toNativeConfig('m', { resources: { cpus: 2 } });
  assert.strictEqual(cfg.resources?.gpu, undefined);
  assert.strictEqual(cfg.resources?.gpuVramMib, undefined);
});
check('forwards cuda to native resources', () => {
  const cfg = toNativeConfig('m', { resources: { cuda: true } });
  assert.strictEqual(cfg.resources?.cuda, true);
});
check('omits cuda when unset (engine default applies)', () => {
  const cfg = toNativeConfig('m', { resources: { cpus: 2 } });
  assert.strictEqual(cfg.resources?.cuda, undefined);
});

// --- cliConfigApiKey: read the smol CLI's stored login from config.toml ---
const withXdg = (fn: (dir: string) => void) => {
  const dir = mkdtempSync(join(tmpdir(), 'smol-sdk-unit-'));
  const prev = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = dir;
  try {
    fn(dir);
  } finally {
    if (prev === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = prev;
    rmSync(dir, { recursive: true, force: true });
  }
};
check('cliConfigApiKey: undefined when no config file exists', () => {
  withXdg(() => assert.strictEqual(cliConfigApiKey(), undefined));
});
check('cliConfigApiKey: reads api_key from the [cloud] section', () => {
  withXdg((dir) => {
    mkdirSync(join(dir, 'smolvm'));
    writeFileSync(
      join(dir, 'smolvm', 'config.toml'),
      '[images]\ndefault_registry = "docker.io"\n\n[cloud]\nendpoint = "https://api.example"\napi_key = "smk_from_cli"\n',
    );
    assert.strictEqual(cliConfigApiKey(), 'smk_from_cli');
  });
});
check('cliConfigApiKey: ignores api_key outside [cloud]', () => {
  withXdg((dir) => {
    mkdirSync(join(dir, 'smolvm'));
    writeFileSync(join(dir, 'smolvm', 'config.toml'), '[other]\napi_key = "smk_wrong"\n');
    assert.strictEqual(cliConfigApiKey(), undefined);
  });
});
check('cliConfigApiKey: empty api_key counts as absent', () => {
  withXdg((dir) => {
    mkdirSync(join(dir, 'smolvm'));
    writeFileSync(join(dir, 'smolvm', 'config.toml'), '[cloud]\napi_key = ""\n');
    assert.strictEqual(cliConfigApiKey(), undefined);
  });
});

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
