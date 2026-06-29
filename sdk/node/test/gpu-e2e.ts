/**
 * GPU e2e: proves the SDK `resources.gpu` flag flows through to libkrun by
 * checking for the virtio-gpu DRM device node under /dev/dri inside the guest.
 *
 * A GPU machine should expose /dev/dri (renderD/card node); a plain machine
 * should not. Run after building the native addon. macOS Apple Silicon (HVF +
 * virglrenderer) or Linux+KVM with a GPU-enabled libkrun.
 */
import { Machine } from '../index';

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

async function driListing(m: Machine): Promise<string> {
  const r = await m.exec(['sh', '-c', 'ls -1 /dev/dri 2>/dev/null || true']);
  return r.stdout.trim();
}

async function main(): Promise<void> {
  console.log('smol SDK GPU e2e\n');

  // 1) GPU machine — expect a DRM render node.
  const gpu = await Machine.create({ resources: { cpus: 2, memoryMb: 1024, gpu: true, gpuVramMib: 512 } });
  console.log(`created GPU machine "${gpu.name}"`);
  try {
    const st = await gpu.state();
    check('GPU machine boots (state running)', st === 'running', `got "${st}"`);
    const dri = await driListing(gpu);
    console.log(`    /dev/dri => ${JSON.stringify(dri)}`);
    check('GPU machine exposes /dev/dri device node', dri.length > 0, 'empty /dev/dri');
    check('GPU machine has a render/card node', /render|card/.test(dri), dri);
  } finally {
    await gpu.delete();
  }

  // 2) Plain machine — same config minus gpu — expect NO /dev/dri.
  const plain = await Machine.create({ resources: { cpus: 2, memoryMb: 1024 } });
  console.log(`created plain machine "${plain.name}"`);
  try {
    const dri = await driListing(plain);
    console.log(`    /dev/dri => ${JSON.stringify(dri)}`);
    check('plain machine has NO /dev/dri (proves the flag matters)', dri.length === 0, dri);
  } finally {
    await plain.delete();
  }

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) process.exit(1);
}

main().catch((err) => {
  console.error('\ngpu e2e crashed:', err);
  process.exit(1);
});
