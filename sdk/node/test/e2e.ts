/**
 * End-to-end smoke test for the embedded `smol` SDK.
 *
 * Exercises the full machine lifecycle against the real engine:
 *   create → run (container) → exec (VM) → write/read file → stream → delete.
 *
 * Run after building the native addon:
 *   cd smol/sdk/node && npm install && npm run test:e2e
 *
 * Requires a host the engine supports (macOS Apple Silicon, or Linux + KVM) and
 * network access (to pull the test image).
 */

import { Machine, ExecutionError } from '../index';

let passed = 0;
let failed = 0;

function check(label: string, ok: boolean, detail = ''): void {
  if (ok) {
    passed++;
    console.log(`  ✓ ${label}`);
  } else {
    failed++;
    console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ''}`);
  }
}

async function main(): Promise<void> {
  console.log('smol SDK e2e\n');

  const t0 = Date.now();
  const machine = await Machine.create({
    resources: { cpus: 2, memoryMb: 1024, network: true },
  });
  console.log(`created machine "${machine.name}" in ${Date.now() - t0}ms`);
  const stRunning = await machine.state();
  check('state() is "running"', stRunning === 'running', `got "${stRunning}"`);

  try {
    // 1) exec directly in the VM
    const echo = await machine.exec(['echo', 'hello-from-vm']);
    check('exec exit code 0', echo.exitCode === 0, `exit=${echo.exitCode} stderr=${echo.stderr}`);
    check('exec stdout', echo.stdout.trim() === 'hello-from-vm', JSON.stringify(echo.stdout));
    check('exec success flag', echo.success === true);

    // 2) run a command in a container image
    const py = await machine.run('python:3.12-alpine', ['python', '-c', 'print(2 ** 10)']);
    check('run exit code 0', py.exitCode === 0, `exit=${py.exitCode} stderr=${py.stderr}`);
    check('run stdout = 1024', py.stdout.trim() === '1024', JSON.stringify(py.stdout));

    // 3) assertSuccess throws on failure
    let threw = false;
    try {
      const bad = await machine.exec(['sh', '-c', 'exit 7']);
      bad.assertSuccess();
    } catch (e) {
      threw = e instanceof ExecutionError && (e as ExecutionError).exitCode === 7;
    }
    check('assertSuccess throws ExecutionError(7)', threw);

    // 4) file round-trip
    const payload = `roundtrip-${Date.now()}`;
    await machine.writeFile('/tmp/smol-e2e.txt', payload);
    const readBack = await machine.readFile('/tmp/smol-e2e.txt');
    check('file round-trip', readBack.toString() === payload, JSON.stringify(readBack.toString()));

    // 5) streaming exec collects an exit event
    let sawExit = false;
    let streamedOut = '';
    for await (const ev of machine.execStream(['sh', '-c', 'echo line1; echo line2'])) {
      if (ev.kind === 'stdout') streamedOut += ev.data;
      if (ev.kind === 'exit') sawExit = true;
    }
    check('stream saw exit event', sawExit);
    check('stream captured stdout', streamedOut.includes('line1') && streamedOut.includes('line2'));

    // 6) images are listable (we pulled one in step 2)
    const images = await machine.listImages();
    check('listImages returns the pulled image', images.some((i) => i.reference.includes('python')));
  } finally {
    await machine.delete();
    const stStopped = await machine.state();
    check('state() after delete is "stopped"', stStopped === 'stopped', `got "${stStopped}"`);
  }

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) process.exit(1);
}

main().catch((err) => {
  console.error('\ne2e crashed:', err);
  process.exit(1);
});
