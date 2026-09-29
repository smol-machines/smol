/**
 * Credential bindings on real local machines: a host-held header reaches an
 * HTTPS echo service while the guest holds nothing, values rotate on restart,
 * and a branch uses its golden's values. Needs KVM and outbound HTTPS.
 */
import { Machine } from '../index';

const ECHO_HOST = 'postman-echo.com';
const opts = { target: 'local' as const, handleSignals: false };
let failures = 0;

function check(label: string, ok: boolean, detail = '') {
  if (!ok) failures++;
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${label}${detail ? `  (${detail})` : ''}`);
}

/** The Authorization header the echo service received from the guest. */
async function echoedAuthorization(m: Machine, method: 'GET' | 'POST' = 'GET'): Promise<string> {
  const url = method === 'GET' ? `https://${ECHO_HOST}/headers` : `https://${ECHO_HOST}/post`;
  const flags = method === 'GET' ? '' : '-X POST -d x';
  // One retry: the echo service is external.
  for (let attempt = 0; attempt < 2; attempt++) {
    const r = await m.exec(['sh', '-c', `curl -sS --fail ${flags} -H "Authorization: Bearer guest-chosen" ${url}`]);
    try {
      return JSON.parse(r.stdout).headers.authorization as string;
    } catch {
      if (attempt === 1) return `no echo: ${(r.stdout + r.stderr).slice(0, 160)}`;
    }
  }
  return '';
}

const binding = {
  name: 'echo',
  allowedHosts: [ECHO_HOST],
  setHeader: 'authorization',
  methods: ['GET'],
};

async function main() {
  const pid = process.pid;
  const m = await Machine.create(
    { name: `cred-${pid}`, image: 'alpine', network: true, credentials: [{ ...binding, value: 'Bearer host-held-1' }] },
    opts,
  );
  try {
    await m.exec(['sh', '-c', 'apk add -q curl']);
    const got = await echoedAuthorization(m);
    check('a GET carries the host-held header, replacing the guest\'s', got === 'Bearer host-held-1', got);
    const post = await echoedAuthorization(m, 'POST');
    check('a method the binding does not allow keeps the guest\'s header', post === 'Bearer guest-chosen', post);
    const leaked = await m.exec(['sh', '-c', 'cat /proc/*/environ 2>/dev/null | tr "\\0" "\\n" | grep -c host-held || true']);
    check('the value is in no guest process environment', leaked.stdout.trim() === '0', leaked.stdout.trim());
    await m.setCredentialValues({ echo: 'Bearer rotated-2' });
    await m.stop();
    await m.start();
    const rotated = await echoedAuthorization(m);
    check('values supplied later take effect on the next start', rotated === 'Bearer rotated-2', rotated);
    let refused = '';
    try {
      await m.setCredentialValues({ nope: 'x' });
    } catch (e) {
      refused = (e as Error).message;
    }
    check('a value for an unknown binding is refused', /no credential binding/.test(refused), refused.slice(0, 100));
  } finally {
    await m.delete().catch(() => {});
  }

  const golden = await Machine.create(
    {
      name: `credg-${pid}`,
      image: 'alpine',
      network: true,
      branchable: true,
      credentials: [{ ...binding, value: 'Bearer golden-3' }],
    },
    opts,
  );
  try {
    await golden.exec(['sh', '-c', 'apk add -q curl']);
    const child = await golden.branch(`credb-${pid}`);
    try {
      const got = await echoedAuthorization(child);
      check('a branch carries its golden\'s header without values of its own', got === 'Bearer golden-3', got);
    } finally {
      await child.delete().catch(() => {});
    }
  } finally {
    await golden.delete().catch(() => {});
  }

  let offline = '';
  try {
    await Machine.create({ name: `credoff-${pid}`, image: 'alpine', credentials: [binding] }, opts);
  } catch (e) {
    offline = (e as Error).message;
  }
  check('credentials on a machine without egress are refused', /network egress/.test(offline), offline.slice(0, 120));

  console.log(`\n${failures === 0 ? 'ALL PASSED' : `${failures} FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error('CREDENTIALS E2E CRASHED:', e);
  process.exit(2);
});
