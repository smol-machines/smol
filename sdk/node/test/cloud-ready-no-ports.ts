/**
 * Regression: a cloud machine with NO published port never flips the `ready`
 * flag on control planes that gate readiness on a port accepting a connection.
 * create() must not hang the full timeout — it confirms the guest agent is
 * reachable (a trivial exec) and returns.
 *
 *   npx tsx test/cloud-ready-no-ports.ts
 */

import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { Machine } from '../index';

let passed = 0;
let failed = 0;
const check = (label: string, ok: boolean, detail = '') => {
  if (ok) { passed++; console.log(`  ✓ ${label}`); }
  else { failed++; console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ''}`); }
};

const seen = { execProbe: false };
const server = createServer((req, res) => {
  const url = req.url ?? '';
  const method = req.method ?? 'GET';
  const json = (code: number, obj: unknown) => {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(obj));
  };
  if (method === 'POST' && url === '/v1/machines') return json(201, { id: 'mX', name: 'np', state: 'stopped', ports: [] });
  if (method === 'POST' && url === '/v1/machines/mX/start') return json(200, { state: 'starting' });
  if (method === 'POST' && url === '/v1/machines/mX/exec') { seen.execProbe = true; return json(200, { exitCode: 0, stdout: '', stderr: '' }); }
  // started, but `ready` STAYS false and there are no ports.
  if (method === 'GET' && url === '/v1/machines/mX') return json(200, { id: 'mX', state: 'started', ready: false, ports: [] });
  if (method === 'DELETE' && url === '/v1/machines/mX') { res.writeHead(204); return res.end(); }
  res.writeHead(404); res.end('no route');
});

async function main(): Promise<void> {
  console.log('smol SDK cloud no-port readiness test (mock /v1)\n');
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const conn = { target: 'cloud' as const, baseUrl: `http://127.0.0.1:${port}`, apiKey: 'smk_t' };

  // Bound it so a regression to the hang fails fast instead of blocking CI 120s.
  const created = await Promise.race([
    Machine.create({ image: 'alpine' }, conn).then((m) => ({ m }), (err) => ({ err })),
    new Promise<'timeout'>((r) => setTimeout(() => r('timeout'), 20_000)),
  ]);

  check('create() returned (did not hang on the never-flipping ready flag)', typeof created === 'object' && 'm' in created,
    created === 'timeout' ? 'still hanging' : String((created as { err?: unknown }).err));
  check('readiness was confirmed via the agent exec probe', seen.execProbe);

  console.log(`\n${passed} passed, ${failed} failed`);
  server.close();
  process.exit(failed > 0 ? 1 : 0);
}

main().catch((e) => { console.error('crashed:', e); server.close(); process.exit(1); });
