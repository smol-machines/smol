/** Command control against real machines (bare VM and image machine):
 *  - a long-running command never blocks other calls on the same machine;
 *  - aborting (AbortSignal) or leaving a stream early kills the command in the
 *    machine — checked with a heartbeat file, since on an image machine each
 *    exec is its own container and `ps` from another exec cannot see it;
 *  - `user` runs a command as that user, and is rejected where unsupported. */
import { Machine } from '../index';

let failures = 0;
const ms = (t: number) => `${(performance.now() - t).toFixed(0)} ms`;
function check(label: string, ok: boolean, detail = '') {
  if (!ok) failures++;
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${label}${detail ? `  (${detail})` : ''}`);
}
const sleep = (n: number) => new Promise((r) => setTimeout(r, n));
/** A command that rewrites a heartbeat file every 200 ms until killed. A
 *  process check that works on image machines too, where each exec is its own
 *  container and `ps` from another exec cannot see it. */
const heartbeat = (name: string) =>
  ['sh', '-c', `mkdir -p /workspace && while true; do date +%s%N > /workspace/${name}; sleep 0.2; done`];
/** True while the heartbeat file keeps changing. */
async function beating(m: Machine, name: string): Promise<boolean> {
  const read = async () => (await m.exec(['cat', `/workspace/${name}`])).stdout.trim();
  const a = await read();
  await sleep(900);
  return a !== '' && (await read()) !== a;
}
async function isAbortError(p: Promise<unknown>): Promise<boolean> {
  try { await p; return false; } catch (e) { return (e as Error).name === 'AbortError'; }
}

async function suite(label: string, m: Machine) {
  console.log(`\n== ${label}`);
  // Regression: plain exec output and exit codes.
  const plain = await m.exec(['sh', '-c', 'echo out; echo err >&2; exit 3']);
  check('exec returns stdout/stderr/exit code', plain.stdout.trim() === 'out' && plain.stderr.trim() === 'err' && plain.exitCode === 3);

  // 1. Concurrency: exec while a long stream runs.
  const long = m.execStream(['sh', '-c', 'sleep 6; echo done-long']);
  const drain = (async () => { let out = ''; for await (const e of long) if (e.kind === 'stdout') out += e.data; return out; })();
  await sleep(500);
  let t = performance.now();
  const during = await m.exec(['echo', 'hi']);
  const concurrentMs = performance.now() - t;
  check('exec stays fast while a stream runs', during.stdout.trim() === 'hi' && concurrentMs < 2000, ms(t));
  t = performance.now();
  await m.writeFile('/tmp/concurrent.txt', 'x');
  check('writeFile stays fast while a stream runs', true, ms(t));
  check('the long stream still completes normally', (await drain).trim() === 'done-long');

  // 2. Abort kills a streaming command, and the process is gone.
  const ac = new AbortController();
  const killed = m.execStream(heartbeat('hb-abort'), { signal: ac.signal });
  const consume = (async () => { for await (const _ of killed) { /* nothing expected */ } })();
  await sleep(1000);
  check('streamed command is running before abort', await beating(m, 'hb-abort'));
  t = performance.now();
  ac.abort();
  check('aborted stream rejects with AbortError', await isAbortError(consume), ms(t));
  await sleep(300);
  check('aborted command is gone from the machine', !(await beating(m, 'hb-abort')));

  // 3. Breaking out of the loop early kills the command.
  const ticker = m.execStream(['sh', '-c', 'mkdir -p /workspace && while true; do echo tick; date +%s%N > /workspace/hb-break; sleep 0.2; done']);
  for await (const e of ticker) { if (e.kind === 'stdout') break; }
  await sleep(300);
  check('breaking out of the loop kills the command', !(await beating(m, 'hb-break')));

  // 4. A plain exec is abortable too.
  const ac2 = new AbortController();
  t = performance.now();
  const slow = m.exec(heartbeat('hb-exec'), { signal: ac2.signal });
  setTimeout(() => ac2.abort(), 1500);
  check('aborted exec rejects with AbortError', await isAbortError(slow), ms(t));
  await sleep(300);
  check('aborted exec is gone from the machine', !(await beating(m, 'hb-exec')));

  // An already-aborted signal starts nothing.
  const pre = new AbortController(); pre.abort();
  check('pre-aborted signal rejects without starting', await isAbortError(m.exec(heartbeat('hb-pre'), { signal: pre.signal })));
  const never = await m.exec(['sh', '-c', 'test -e /workspace/hb-pre && echo exists || echo absent']);
  check('pre-aborted command never started', never.stdout.trim() === 'absent');

  // 6. stop() is not stuck behind a running command.
  const bg = m.execStream(['sh', '-c', 'sleep 65']);
  const bgDone = (async () => { try { for await (const _ of bg) {} } catch { /* stop ends it */ } })();
  await sleep(500);
  t = performance.now();
  await m.stop();
  check('stop() is not blocked by a running command', performance.now() - t < 15000, ms(t));
  await bgDone;
}

async function main() {
  const opts = { target: 'local' as const, handleSignals: false };
  const bare = await Machine.create({ name: `val-bare-${process.pid}` }, opts);
  try {
    await suite('bare VM', bare);
    await bare.start();
    let err = '';
    try { await bare.exec(['id'], { user: 'nobody' }); } catch (e) { err = String((e as Error).message); }
    check('bare VM rejects a per-exec user clearly', /image machine/.test(err), err.slice(0, 80));
  } finally { await bare.delete().catch(() => {}); }

  const img = await Machine.create({ name: `val-img-${process.pid}`, image: 'alpine', network: true }, opts);
  try {
    const asUser = await img.exec(['id', '-u'], { user: 'nobody' });
    check('exec runs as the requested user', asUser.stdout.trim() === '65534', asUser.stdout.trim());
    const asRoot = await img.exec(['id', '-u']);
    check('exec without user still runs as root', asRoot.stdout.trim() === '0');
    let streamed = '';
    for await (const e of img.execStream(['id', '-u'], { user: 'nobody' })) if (e.kind === 'stdout') streamed += e.data;
    check('execStream runs as the requested user', streamed.trim() === '65534', streamed.trim());
    let runErr = '';
    try { await img.run('alpine', ['id'], { user: 'nobody' }); } catch (e) { runErr = String((e as Error).message); }
    check('run() rejects a per-exec user clearly', /not supported by run/.test(runErr), runErr.slice(0, 80));
    await suite('image machine (alpine)', img);
  } finally { await img.delete().catch(() => {}); }

  console.log(`\nexec-control-e2e: ${failures === 0 ? 'passed' : `${failures} failed`}`);
  process.exit(failures === 0 ? 0 : 1);
}
main().catch((e) => { console.error('exec-control-e2e crashed:', e); process.exit(2); });
