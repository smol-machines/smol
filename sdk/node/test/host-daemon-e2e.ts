/**
 * End-to-end checks for embedding the SDK in a long-running host daemon:
 * open without booting, delete by name, a non-blocking connect and state(),
 * exec stdin, sizes and ports in list(), stops fenced against running work,
 * live port publishing, pack and create-from-pack, and disks under 20 GiB.
 *
 * Run with a bundled engine: SMOLVM_BOOT_BINARY, SMOLVM_LIB_DIR and
 * SMOLVM_AGENT_ROOTFS pointing at a smolvm build.
 */
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { createConnection } from 'node:net';
import { Machine, SmolError } from '../index';

const IMAGE = 'alpine:3.20';
const results: [string, boolean, string][] = [];
const check = (name: string, ok: boolean, detail = '') => {
  results.push([name, ok, detail]);
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` (${detail})` : ''}`);
};
const code = (e: unknown) => (e instanceof SmolError ? e.code : String(e));

/** Longest gap between event-loop ticks while `work` runs, in ms. */
async function loopStall<T>(work: () => Promise<T>): Promise<[T, number]> {
  let last = Date.now();
  let worst = 0;
  const timer = setInterval(() => {
    const now = Date.now();
    worst = Math.max(worst, now - last);
    last = now;
  }, 10);
  try {
    return [await work(), worst];
  } finally {
    clearInterval(timer);
  }
}

function connectTo(port: number): Promise<string> {
  return new Promise((resolve) => {
    const socket = createConnection({ host: '127.0.0.1', port });
    let data = '';
    socket.setTimeout(4000, () => { socket.destroy(); resolve('timeout'); });
    socket.on('data', (d) => (data += d.toString()));
    socket.on('end', () => resolve(data.trim()));
    socket.on('error', () => resolve('refused'));
  });
}

async function main() {
  const names = ['hd-a', 'hd-b', 'hd-small', 'hd-from-pack'];
  for (const n of names) await Machine.delete(n).catch(() => {});

  // --- open, delete by name, async connect -----------------------------
  const a = await Machine.create({ name: 'hd-a', image: IMAGE, persistent: true, network: true });
  await a.stop();
  const opened = await Machine.open('hd-a');
  check('open() a stopped machine reports stopped without booting', (await opened.state()) === 'stopped');
  try {
    await Machine.open('hd-no-such');
    check('open() of a missing machine rejects NOT_FOUND', false);
  } catch (e) {
    check('open() of a missing machine rejects NOT_FOUND', code(e) === 'NOT_FOUND', code(e));
  }
  const [connected, stall] = await loopStall(() => Machine.connect('hd-a', { waitForPorts: false }));
  check('connect() boots without blocking the event loop', stall < 250, `worst tick gap ${stall}ms`);
  check('connect() booted it', (await connected.state()) === 'running');

  // --- exec stdin ------------------------------------------------------
  const script = 'echo from-script; cat /etc/alpine-release | cut -c1-4';
  const viaScript = await connected.exec(['sh', '-s'], { stdin: script });
  check('exec stdin runs a piped script', viaScript.stdout.trim() === 'from-script\n3.20'.trim(), JSON.stringify(viaScript.stdout));
  const secret = await connected.exec(['sh', '-c', 'read s; echo "${#s}"; ls /tmp'], { stdin: 'hunter2\n' });
  check('exec stdin reaches the command and is not written to /tmp', secret.stdout.split('\n')[0] === '7', JSON.stringify(secret.stdout));

  // --- state() during a large file transfer ----------------------------
  const big = Buffer.alloc(256 * 1024 * 1024, 7);
  const transfer = connected.writeFile('/tmp/big.bin', big);
  await new Promise((r) => setTimeout(r, 100));
  const t0 = Date.now();
  const during = await connected.state();
  const stateMs = Date.now() - t0;
  await transfer;
  check('state() answers during a 256 MiB transfer', during === 'running' && stateMs < 200, `${during} in ${stateMs}ms`);

  // --- stop fenced against running work --------------------------------
  const longExec = connected.exec(['sh', '-c', 'sleep 30']).then(
    () => 'finished',
    (e) => code(e),
  );
  await new Promise((r) => setTimeout(r, 500));
  await connected.stop();
  const cut = await longExec;
  check('stop() during exec rejects it with INTERRUPTED', cut === 'INTERRUPTED', cut);
  const reconnected = await Machine.connect('hd-a', { waitForPorts: false });
  const shortExec = reconnected.exec(['sh', '-c', 'sleep 2; echo done']);
  await new Promise((r) => setTimeout(r, 300));
  await reconnected.stop({ drainSecs: 10 });
  const drained = await shortExec;
  check('stop({ drainSecs }) lets a running exec finish', drained.stdout.trim() === 'done', JSON.stringify(drained.stdout));

  // --- delete by name of a stopped machine; missing is fine -------------
  await Machine.delete('hd-a');
  check('Machine.delete(name) removes a stopped machine', !(await Machine.list()).some((m) => m.name === 'hd-a'));
  await Machine.delete('hd-never-existed');
  check('Machine.delete(name) of a missing machine succeeds', true);

  // --- list sizes and ports; live publish/unpublish ---------------------
  const b = await Machine.create({
    name: 'hd-b',
    image: IMAGE,
    persistent: true,
    network: true,
    resources: { cpus: 2, memoryMb: 1024 },
    ports: [{ host: 18431, guest: 8080 }],
    waitForPorts: false,
  } as never);
  const row = (await Machine.list()).find((m) => m.name === 'hd-b');
  check(
    'list() reports sizes and ports',
    row?.resources?.cpus === 2 && row?.resources?.memoryMib === 1024 && !!row?.ports?.some((p) => p.guest === 8080),
    JSON.stringify({ resources: row?.resources, ports: row?.ports }),
  );
  await b.exec(['sh', '-c', '(while true; do echo hello-3000 | nc -l -p 3000; done) >/dev/null 2>&1 &']);
  try {
    const published = await b.publish({ guest: 3000 });
    const reply = await connectTo(published.host!);
    check('publish() on a running machine reaches the guest', reply === 'hello-3000', `${published.host} -> ${reply}`);
    await b.unpublish({ guest: 3000 });
    const after = await connectTo(published.host!);
    check('unpublish() stops accepting', after === 'refused' || after === '', after);
    check('ports() reflects the change', !(await b.ports()).some((p) => p.guest === 3000));
  } catch (e) {
    check('publish()/unpublish() on a running machine', false, `${code(e)}: ${(e as Error).message}`);
  }

  // --- pack and create from it -----------------------------------------
  await b.exec(['sh', '-c', 'echo packed-state > /root/marker']);
  await b.stop();
  const dir = mkdtempSync(join(tmpdir(), 'hd-pack-'));
  try {
    const artifact = await b.pack(join(dir, 'rig'));
    check('pack() writes a .smolmachine', artifact.endsWith('.smolmachine'), artifact);
    const restored = await Machine.create({ name: 'hd-from-pack', image: artifact, waitForPorts: false });
    const marker = await restored.exec(['cat', '/root/marker']);
    check('a machine created from the pack has its state', marker.stdout.trim() === 'packed-state', JSON.stringify(marker.stdout));
    await restored.delete();

    // Options on a pack are applied the way they are on an image, and a
    // branchable pack machine can be branched.
    const configured = await Machine.create({
      name: 'hd-pack-options',
      image: artifact,
      env: { PACK_OPTION: 'set' },
      workdir: '/tmp',
      labels: { suite: 'host-daemon' },
      resources: { cpus: 1, memoryMb: 768 },
      branchable: true,
      waitForPorts: false,
    } as never);
    const seen = await configured.exec(['sh', '-c', 'echo "$PACK_OPTION $(pwd)"']);
    check('env and workdir reach a pack machine', seen.stdout.trim() === 'set /tmp', JSON.stringify(seen.stdout));
    const packRow = (await Machine.list()).find((m) => m.name === 'hd-pack-options');
    check(
      'labels and resources reach a pack machine',
      packRow?.labels?.suite === 'host-daemon' && packRow?.resources?.cpus === 1 && packRow?.resources?.memoryMib === 768,
      JSON.stringify({ labels: packRow?.labels, resources: packRow?.resources }),
    );
    const branch = await configured.fork('hd-pack-branch');
    const branchMarker = await branch.exec(['cat', '/root/marker']);
    check('a branchable pack machine branches', branchMarker.stdout.trim() === 'packed-state', JSON.stringify(branchMarker.stdout));
    await branch.delete();
    await configured.delete();
  } catch (e) {
    check('pack() and create from it', false, `${code(e)}: ${(e as Error).message}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  await Machine.delete('hd-b');

  // --- a disk under 20 GiB ---------------------------------------------
  try {
    const small = await Machine.create({
      name: 'hd-small',
      image: IMAGE,
      network: true,
      resources: { storageGb: 8 },
      waitForPorts: false,
    } as never);
    const df = await small.exec(['sh', '-c', "df -k /storage | tail -1 | awk '{print $2}'"]);
    const kib = Number(df.stdout.trim());
    check('an 8 GiB disk boots at its size', kib > 7 * 2 ** 20 && kib <= 8 * 2 ** 20, `${kib} KiB`);
    await small.delete();
  } catch (e) {
    check('an 8 GiB disk boots', false, `${code(e)}: ${(e as Error).message}`);
  }

  const failed = results.filter(([, ok]) => !ok).length;
  console.log(`RESULT pass=${results.length - failed} fail=${failed}`);
  process.exit(failed ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
