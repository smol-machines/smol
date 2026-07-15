/** Pure-unit tests — no VM boot, no network. Covers the error-parsing seam and
 *  cloud path encoding, the two places most likely to silently regress. */
import assert from 'node:assert';
import { wrapNativeError, SmolError } from '../errors';
import { encodePath, toNativeConfig } from '../transport';

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

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
