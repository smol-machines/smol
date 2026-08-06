/** Pure-unit tests — no VM boot, no network. Covers the error-parsing seam and
 *  cloud path encoding, the two places most likely to silently regress. */
import assert from 'node:assert';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { wrapNativeError, SmolError } from '../errors';
import { adapterSha256, RolloutClient } from '../rollout';
import { cliConfigApiKey, encodePath, resolveNetwork, toNativeConfig } from '../transport';

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

check('rollout adapter digest matches the engine contract', () => {
  const dir = mkdtempSync(join(tmpdir(), 'smol-rollout-unit-'));
  try {
    writeFileSync(join(dir, 'adapter_config.json'), '{}');
    writeFileSync(join(dir, 'adapter_model.safetensors'), 'weights');
    assert.strictEqual(
      adapterSha256(dir),
      '26d1c7593b9650cb489a9a1fe2fad9def32c75ec2685cf8261c3c0fa3b73e315',
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

check('rollout job preserves token prompts and sampling', () => {
  const client = new RolloutClient('http://127.0.0.1:8080/api/v1', 'qwen');
  assert.deepStrictEqual(
    client.job({
      idempotencyKey: 'policy-a-step-4',
      policy: 'policy-a',
      version: 'step-4',
      prompts: [[1, 2, 3]],
      sampling: { maxTokens: 64, temperature: 0.8, topP: 0.95 },
    }),
    {
      idempotencyKey: 'policy-a-step-4',
      policy: 'policy-a',
      version: 'step-4',
      prompts: [{ tokenIds: [1, 2, 3] }],
      sampling: { maxTokens: 64, temperature: 0.8, topP: 0.95, n: 1 },
    },
  );
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

async function checkAsync(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (e) {
    failed++;
    console.log(`  ✗ ${name}: ${(e as Error).message}`);
  }
}

async function finish() {
  await checkAsync('publishes a device policy with a canonical token', async () => {
    const requests: Array<{ url: string; init?: RequestInit }> = [];
    const fetchMock: typeof fetch = async (input, init) => {
      requests.push({ url: String(input), init });
      return new Response(JSON.stringify({ source: 'device' }), {
        status: 201,
        headers: { 'content-type': 'application/json' },
      });
    };
    const client = new RolloutClient('http://127.0.0.1:8080/api/v1', 'qwen', {
      fetch: fetchMock,
    });
    const result = await client.publishDevicePolicy(
      'policy-a',
      'step-4',
      Uint8Array.from({ length: 32 }, (_, index) => index),
      true,
    );
    assert.strictEqual(result.source, 'device');
    assert.strictEqual(requests.length, 1);
    assert.strictEqual(
      requests[0].url,
      'http://127.0.0.1:8080/api/v1/rollout-executors/qwen/device-policies',
    );
    assert.deepStrictEqual(JSON.parse(String(requests[0].init?.body)), {
      policy: 'policy-a',
      version: 'step-4',
      tensorBundleToken: Buffer.from(
        Uint8Array.from({ length: 32 }, (_, index) => index),
      ).toString('hex'),
      retainPrevious: true,
    });
  });

  await checkAsync('retries an ambiguous device publication once', async () => {
    let calls = 0;
    const fetchMock: typeof fetch = async () => {
      calls++;
      if (calls === 1) throw new Error('response lost');
      return new Response(JSON.stringify({ source: 'device' }), { status: 201 });
    };
    const client = new RolloutClient('http://127.0.0.1:8080/api/v1', 'qwen', {
      fetch: fetchMock,
    });
    await client.publishDevicePolicy('policy-a', 'step-4', 'ab'.repeat(32));
    assert.strictEqual(calls, 2);
  });

  await checkAsync('rejects an invalid device publication token', async () => {
    const client = new RolloutClient('http://127.0.0.1:8080/api/v1', 'qwen');
    await assert.rejects(
      client.publishDevicePolicy('policy-a', 'step-4', 'zz'.repeat(32)),
      /exactly 32 hexadecimal bytes/,
    );
  });

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) process.exitCode = 1;
}

void finish();

// --- network: asked for at either level, honoured at both ---
check('a top-level network is honoured instead of dropped', () => {
  // This is the config people write first. It used to be ignored outright, so
  // the machine came up with no network and nothing said why.
  assert.strictEqual(resolveNetwork({ image: 'alpine', network: true }), true);
  assert.strictEqual(
    toNativeConfig('m', { image: 'alpine', network: true }).resources?.network,
    true,
  );
});

check('resources.network still wins when both are given', () => {
  assert.strictEqual(
    resolveNetwork({ image: 'alpine', network: true, resources: { network: false } }),
    false,
  );
});

check('the canonical resources.network is unchanged', () => {
  assert.strictEqual(
    toNativeConfig('m', { image: 'alpine', resources: { network: true } }).resources?.network,
    true,
  );
});

check('asking for neither leaves network unset', () => {
  assert.strictEqual(resolveNetwork({ image: 'alpine' }), undefined);
  assert.strictEqual(toNativeConfig('m', { image: 'alpine' }).resources, undefined);
});
