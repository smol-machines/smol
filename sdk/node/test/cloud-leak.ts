/**
 * Regression: a cloud machine that fails to become ready must NOT leak — the
 * SDK must delete the just-created machine before surfacing the error.
 *
 *   npx tsx test/cloud-leak.ts
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

const seen = { deleted: false };
// Mock: create OK, start OK, but the machine reports `error` state → readiness fails.
const server = createServer((req, res) => {
  const url = req.url ?? '';
  const method = req.method ?? 'GET';
  const json = (code: number, obj: unknown) => {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(obj));
  };
  if (method === 'POST' && url === '/v1/machines') return json(200, { id: 'mX', name: 'leaky', state: 'created' });
  if (method === 'POST' && url === '/v1/machines/mX/start') return json(200, { state: 'starting' });
  if (method === 'GET' && url === '/v1/machines/mX') return json(200, { id: 'mX', state: 'error' });
  if (method === 'DELETE' && url === '/v1/machines/mX') { seen.deleted = true; res.writeHead(204); return res.end(); }
  res.writeHead(404); res.end('no route');
});

async function main(): Promise<void> {
  console.log('smol SDK cloud leak-on-failed-start test (mock /v1)\n');
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;

  let threw = false;
  let code = '';
  try {
    await Machine.create({ image: 'alpine' }, { target: 'cloud', baseUrl: `http://127.0.0.1:${port}`, apiKey: 'smk_t' });
  } catch (e) {
    threw = true;
    code = e instanceof SmolError ? e.code : '';
  }
  check('create() rejects when the machine enters error state', threw, code);
  check('orphan machine was deleted (no leak)', seen.deleted);

  console.log(`\n${passed} passed, ${failed} failed`);
  server.close();
  if (failed > 0) process.exit(1);
}

main().catch((e) => { console.error('cloud-leak crashed:', e); server.close(); process.exit(1); });
