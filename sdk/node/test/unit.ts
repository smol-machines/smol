/** Pure-unit tests — no VM boot, no network. Covers the error-parsing seam and
 *  cloud path encoding, the two places most likely to silently regress. */
import assert from 'node:assert';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { wrapNativeError, SmolError } from '../errors';
import { adapterSha256, RolloutClient } from '../rollout';
import { cliConfigApiKey, encodePath, resolveNetwork, selectsCloud, toNativeConfig } from '../transport';
import { wireDefaultHardening } from '../assets';

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
check('forwards the base image to the native machine', () => {
  const cfg = toNativeConfig('m', { image: 'python:3.12-slim' });
  assert.strictEqual(cfg.image, 'python:3.12-slim');
});
check('omits the base image when unset', () => {
  const cfg = toNativeConfig('m', {});
  assert.strictEqual(cfg.image, undefined);
});
check('forwards forkable lifecycle to the native machine', () => {
  assert.strictEqual(toNativeConfig('m', { forkable: true }).forkable, true);
  assert.strictEqual(toNativeConfig('m', {}).forkable, undefined);
});
check('prefers branchable while retaining lifecycle aliases', () => {
  assert.strictEqual(toNativeConfig('m', { branchable: true }).forkable, true);
  assert.strictEqual(
    toNativeConfig('m', { branchable: false, forkable: true }).forkable,
    false,
  );
  assert.strictEqual(toNativeConfig('m', { checkpoint: true }).forkable, true);
});
check('forwards image workload env and workdir to the local engine', () => {
  const cfg = toNativeConfig('m', {
    image: 'example/service:latest',
    env: { SESSION: 'golden' },
    workdir: '/workspace',
  });
  assert.deepStrictEqual(cfg.env, [{ key: 'SESSION', value: 'golden' }]);
  assert.strictEqual(cfg.workdir, '/workspace');
});
check('forwards staged mount mode to the local engine', () => {
  const cfg = toNativeConfig('m', {
    mounts: [{ source: '/host/work', target: '/work', staged: true }],
  });
  assert.deepStrictEqual(cfg.mounts, [
    { source: '/host/work', target: '/work', readOnly: undefined, staged: true },
  ]);
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

// --- selectsCloud: a stored CLI login must not flip the default off local ---
const withCloudToken = (value: string | undefined, fn: () => void) => {
  const prev = process.env.SMOL_CLOUD_TOKEN;
  if (value === undefined) delete process.env.SMOL_CLOUD_TOKEN;
  else process.env.SMOL_CLOUD_TOKEN = value;
  try {
    fn();
  } finally {
    if (prev === undefined) delete process.env.SMOL_CLOUD_TOKEN;
    else process.env.SMOL_CLOUD_TOKEN = prev;
  }
};
check('selectsCloud: defaults to local with no explicit credential', () => {
  withCloudToken(undefined, () => assert.strictEqual(selectsCloud({}), false));
});
check('selectsCloud: a CLI login on disk does NOT select cloud', () => {
  // The regression: `smol auth login` writes config.toml, and reading it here
  // used to send every default create to the cloud.
  withXdg((dir) => {
    mkdirSync(join(dir, 'smolvm'));
    writeFileSync(join(dir, 'smolvm', 'config.toml'), '[cloud]\napi_key = "smk_from_cli"\n');
    withCloudToken(undefined, () => {
      assert.strictEqual(cliConfigApiKey(), 'smk_from_cli'); // the key IS on disk
      assert.strictEqual(selectsCloud({}), false); // and is still not a target vote
    });
  });
});
check('selectsCloud: an explicit apiKey selects cloud', () => {
  withCloudToken(undefined, () =>
    assert.strictEqual(selectsCloud({ apiKey: 'smk_explicit' }), true),
  );
});
check('selectsCloud: SMOL_CLOUD_TOKEN selects cloud', () => {
  withCloudToken('smk_env', () => assert.strictEqual(selectsCloud({}), true));
});
check('selectsCloud: an explicit local target beats every credential', () => {
  withCloudToken('smk_env', () =>
    assert.strictEqual(selectsCloud({ target: 'local', apiKey: 'smk_explicit' }), false),
  );
});
check('selectsCloud: an explicit cloud target needs no credential to select', () => {
  withCloudToken(undefined, () => assert.strictEqual(selectsCloud({ target: 'cloud' }), true));
});

// --- wireDefaultHardening: confine the spawned VMM unless told otherwise ---
const withHardeningEnv = (
  seccomp: string | undefined,
  landlock: string | undefined,
  fn: () => void,
) => {
  const prev = [process.env.SMOLVM_SECCOMP, process.env.SMOLVM_LANDLOCK];
  const set = (k: string, v: string | undefined) => {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  };
  set('SMOLVM_SECCOMP', seccomp);
  set('SMOLVM_LANDLOCK', landlock);
  try {
    fn();
  } finally {
    set('SMOLVM_SECCOMP', prev[0]);
    set('SMOLVM_LANDLOCK', prev[1]);
  }
};
check('wireDefaultHardening: an explicit value always wins', () => {
  withHardeningEnv('off', 'off', () => {
    wireDefaultHardening();
    assert.strictEqual(process.env.SMOLVM_SECCOMP, 'off');
    assert.strictEqual(process.env.SMOLVM_LANDLOCK, 'off');
  });
});
check('wireDefaultHardening: enforces by default on Linux, no-ops elsewhere', () => {
  withHardeningEnv(undefined, undefined, () => {
    wireDefaultHardening();
    // Both knobs are Linux-only in the engine; setting them on macOS/Windows
    // would imply a confinement the helper does not apply.
    const expected = process.platform === 'linux' ? 'enforce' : undefined;
    assert.strictEqual(process.env.SMOLVM_SECCOMP, expected);
    assert.strictEqual(process.env.SMOLVM_LANDLOCK, expected);
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

  await checkAsync('discovers a rollout lease and cohort without constructor wiring', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'smol-rollout-lease-'));
    const assignment = join(dir, 'fork-env');
    writeFileSync(
      assignment,
      'SMOLVM_ROLLOUT_URL=http://100.96.0.1:10081/api/v1/rollout-executors/fused\n'
        + 'SMOLVM_ROLLOUT_TOKEN=lease-id.secret\n'
        + 'SMOLVM_ROLLOUT_EXECUTOR=fused\n'
        + 'SMOLVM_ROLLOUT_POLICY=experiment-a\n'
        + 'SMOLVM_FORK_BATCH_ID=batch-a\n'
        + 'SMOLVM_FORK_BATCH_SIZE=2\n',
    );
    const requests: Array<{ url: string; init?: RequestInit }> = [];
    const fetchMock: typeof fetch = async (input, init) => {
      requests.push({ url: String(input), init });
      return new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } });
    };
    try {
      const client = new RolloutClient(undefined, undefined, {
        forkEnvPath: assignment,
        fetch: fetchMock,
      });
      assert.strictEqual(client.apiUrl, 'http://100.96.0.1:10081/api/v1');
      assert.strictEqual(client.executor, 'fused');
      assert.strictEqual(client.leasePolicy, 'experiment-a');
      await client.generate({
        idempotencyKey: 'request',
        policy: client.leasePolicy as string,
        prompts: ['hello'],
        sampling: { maxTokens: 1 },
      });
      assert.strictEqual(new Headers(requests[0].init?.headers).get('authorization'), 'Bearer lease-id.secret');
      const body = JSON.parse(String(requests[0].init?.body));
      assert.match(body.cohort.id, /^fork-[0-9a-f]{32}$/);
      assert.deepStrictEqual(
        { size: body.cohort.size, maxWaitMs: body.cohort.maxWaitMs },
        { size: 2, maxWaitMs: 250 },
      );
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  await checkAsync('automatically aligns batch-fork cohorts and preserves explicit cohorts', async () => {
    const previousId = process.env.SMOLVM_FORK_BATCH_ID;
    const previousSize = process.env.SMOLVM_FORK_BATCH_SIZE;
    process.env.SMOLVM_FORK_BATCH_ID = 'batch-a';
    process.env.SMOLVM_FORK_BATCH_SIZE = '2';
    const bodies: unknown[] = [];
    const fetchMock: typeof fetch = async (_input, init) => {
      bodies.push(JSON.parse(String(init?.body)));
      return new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } });
    };
    try {
      const first = new RolloutClient('http://host/api/v1', 'fused', { fetch: fetchMock });
      const second = new RolloutClient('http://host/api/v1', 'fused', { fetch: fetchMock });
      const common = { policy: 'policy', prompts: ['hello'], sampling: { maxTokens: 1 } };
      await first.generate({ ...common, idempotencyKey: 'learner-0-step-0' });
      await second.generate({ ...common, idempotencyKey: 'learner-1-step-0' });
      assert.deepStrictEqual(
        (bodies[0] as { cohort: unknown }).cohort,
        (bodies[1] as { cohort: unknown }).cohort,
      );
      const explicit = first.job({
        ...common,
        idempotencyKey: 'explicit',
        cohort: { id: 'controller-round', size: 2, maxWaitMs: 100 },
      });
      assert.deepStrictEqual(explicit.cohort, {
        id: 'controller-round',
        size: 2,
        maxWaitMs: 100,
      });
    } finally {
      if (previousId === undefined) delete process.env.SMOLVM_FORK_BATCH_ID;
      else process.env.SMOLVM_FORK_BATCH_ID = previousId;
      if (previousSize === undefined) delete process.env.SMOLVM_FORK_BATCH_SIZE;
      else process.env.SMOLVM_FORK_BATCH_SIZE = previousSize;
    }
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
