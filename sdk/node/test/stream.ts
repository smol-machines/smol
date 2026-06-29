/**
 * Local e2e for LIVE streaming exec — proves output arrives incrementally, not
 * buffered. Needs the native build + boot env (SMOLVM_BOOT_BINARY, SMOLVM_LIB_DIR).
 *
 *   npx tsx test/stream.ts
 */

import { Machine } from '../index';

let passed = 0;
let failed = 0;
const check = (label: string, ok: boolean, detail = '') => {
  if (ok) { passed++; console.log(`  ✓ ${label}`); }
  else { failed++; console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ''}`); }
};

async function main(): Promise<void> {
  console.log('smol Node SDK streaming exec test (incrementality)\n');
  const m = await Machine.create({ resources: { cpus: 1, memoryMb: 512 } });
  try {
    const events: Array<[number, { kind: string; data?: string; exitCode?: number }]> = [];
    for await (const ev of m.execStream(['sh', '-c', 'echo AAA; sleep 1; echo BBB'])) {
      events.push([Date.now(), ev as { kind: string; data?: string; exitCode?: number }]);
    }
    const out = events.filter(([, e]) => e.kind === 'stdout').map(([, e]) => e.data ?? '').join('');
    check('stdout contains AAA', out.includes('AAA'), out);
    check('stdout contains BBB', out.includes('BBB'), out);

    const exits = events.filter(([, e]) => e.kind === 'exit');
    check('single exit event, code 0', exits.length === 1 && exits[0][1].exitCode === 0, JSON.stringify(exits));

    const tA = events.find(([, e]) => e.kind === 'stdout' && (e.data ?? '').includes('AAA'))?.[0];
    const tB = events.find(([, e]) => e.kind === 'stdout' && (e.data ?? '').includes('BBB'))?.[0];
    const gap = tA != null && tB != null ? (tB - tA) / 1000 : -1;
    check('output streamed incrementally (>=0.5s gap)', gap >= 0.5, `gap=${gap.toFixed(3)}s`);
  } finally {
    await m.delete();
  }
  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) process.exit(1);
}

main().catch((e) => { console.error('stream test crashed:', e); process.exit(1); });
