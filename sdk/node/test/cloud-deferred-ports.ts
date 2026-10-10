/** Regression: create and connect can reach the cloud guest agent before a published service starts. */
import { strict as assert } from 'node:assert';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { Machine } from '../index';

let nextId = 0;
const records = new Map<string, { listener: boolean; probes: number; deleted: boolean }>();
const server = createServer((req, res) => {
  const url = req.url ?? '';
  const method = req.method ?? 'GET';
  const match = /^\/v1\/machines\/([^/]+)(?:\/(start|exec))?$/.exec(url);
  const json = (status: number, body: unknown) => {
    res.writeHead(status, { 'content-type': 'application/json' });
    res.end(JSON.stringify(body));
  };
  if (method === 'POST' && url === '/v1/machines') {
    const id = `deferred-${++nextId}`;
    records.set(id, { listener: false, probes: 0, deleted: false });
    return json(201, { id, name: id, state: 'stopped' });
  }
  if (!match) return json(404, { error: 'unknown path' });
  const id = match[1]!;
  const record = records.get(id);
  if (!record) return json(404, { error: 'missing machine' });
  if (method === 'GET' && !match[2]) return json(200, {
    id, name: id, state: 'started', ready: record.listener,
    ports: [{ port: 18999, hostPort: 18999 }],
  });
  if (method === 'POST' && match[2] === 'start') return json(200, { state: 'starting' });
  if (method === 'POST' && match[2] === 'exec') {
    record.probes++;
    // A started machine may still have an agent that cannot execute commands.
    if (record.probes === 1) return json(200, { exitCode: 1, stdout: '', stderr: 'booting' });
    return json(200, { exitCode: 0, stdout: 'ok', stderr: '' });
  }
  if (method === 'DELETE' && !match[2]) {
    record.deleted = true;
    res.writeHead(204);
    return res.end();
  }
  return json(404, { error: 'unknown operation' });
});

async function main(): Promise<void> {
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const baseUrl = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  const conn = { target: 'cloud' as const, baseUrl, apiKey: 'smk_test' };
  try {
    const machine = await Machine.create({ image: 'alpine', waitForPorts: false, ports: [{ guest: 18999 }] }, conn);
    const record = records.get(machine.id)!;
    assert.equal(record.listener, false, 'created before port opens');
    assert.equal(record.probes, 2, 'waited for a successful guest-agent command');
    assert.equal((await machine.exec(['sh', '-c', 'echo ok'])).exitCode, 0);
    record.listener = true;
    await machine.waitUntilReady({ timeoutMs: 1000 });
    await machine.delete();
    assert.equal(record.deleted, true);

    const fromOptions = await Machine.create({ image: 'alpine', ports: [{ guest: 18999 }] }, { ...conn, waitForPorts: false });
    assert.equal(records.get(fromOptions.id)?.probes, 2, 'connection option also defers ports');
    await fromOptions.delete();

    const beforeDefault = nextId;
    const defaultCreate = Machine.create({ image: 'alpine', ports: [{ guest: 18999 }] }, conn);
    // Wait for the create request before checking its readiness behavior.
    for (let i = 0; nextId === beforeDefault && i < 50; i++) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert.ok(nextId > beforeDefault, 'default create reached the server');
    const pending = records.get(`deferred-${nextId}`)!;
    assert.equal(pending.listener, false);
    assert.equal(pending.probes, 0, 'default path does not bypass port readiness');
    pending.listener = true;
    const defaultMachine = await defaultCreate;
    await defaultMachine.delete();

    const existing = `deferred-${++nextId}`;
    const attached = { listener: false, probes: 0, deleted: false };
    records.set(existing, attached);
    const connected = await Machine.connect(existing, { ...conn, waitForPorts: false });
    assert.equal(attached.probes, 2, 'connect waits for a successful agent probe');
    assert.equal(connected.id, existing);
    await connected.delete();
    console.log('cloud deferred port readiness: passed');
  } finally {
    server.close();
  }
}

main().catch((e) => { console.error(e); server.close(); process.exitCode = 1; });
