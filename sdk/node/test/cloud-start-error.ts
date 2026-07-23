/**
 * Regression: when a cloud machine fails to start and then enters `error`, the
 * SDK must surface WHY start failed (the machine record carries no error detail)
 * — not just an opaque "entered error state" — and still delete the orphan.
 *
 *   npx tsx test/cloud-start-error.ts
 */

import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { Machine, SmolError } from '../index';

let passed = 0;
let failed = 0;
const check = (label: string, ok: boolean, detail = '') => {
  if (ok) { passed++; console.log(`  ✓ ${label}`); }
  else { failed++; console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ''}`); }
};

const REASON = 'pull image: crane manifest failed: manifest unknown';
const seen = { deleted: false };
// Mock: create OK, start FAILS with the real reason, machine reports `error`.
const server = createServer((req, res) => {
  const url = req.url ?? '';
  const method = req.method ?? 'GET';
  const json = (code: number, obj: unknown) => {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(obj));
  };
  if (method === 'POST' && url === '/v1/machines') return json(201, { id: 'mX', name: 'boom', state: 'stopped' });
  if (method === 'POST' && url === '/v1/machines/mX/start') return json(500, { error: REASON });
  if (method === 'GET' && url === '/v1/machines/mX') return json(200, { id: 'mX', state: 'error' });
  if (method === 'DELETE' && url === '/v1/machines/mX') { seen.deleted = true; res.writeHead(204); return res.end(); }
  res.writeHead(404); res.end('no route');
});

async function main(): Promise<void> {
  console.log('smol SDK cloud start-failure surfacing test (mock /v1)\n');
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;

  let threw = false;
  let message = '';
  try {
    await Machine.create({ image: 'alpine' }, { target: 'cloud', baseUrl: `http://127.0.0.1:${port}`, apiKey: 'smk_t' });
  } catch (e) {
    threw = true;
    message = e instanceof SmolError ? e.message : String(e);
  }
  check('create() rejects when the machine enters error state', threw);
  check('error message surfaces the start-failure reason', message.includes(REASON), message);
  check('orphan machine was deleted (no leak)', seen.deleted);

  console.log(`\n${passed} passed, ${failed} failed`);
  server.close();
  if (failed > 0) process.exit(1);
}

main().catch((e) => { console.error('cloud-start-error crashed:', e); server.close(); process.exit(1); });
