/** Run with `npm exec -- tsx test/local-agent-ready.ts` on a KVM host. */
import assert from "node:assert/strict";
import net from "node:net";
import { Machine } from "../machine";

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const listener = net.createServer();
    listener.once("error", reject);
    listener.listen(0, "127.0.0.1", () => {
      const address = listener.address();
      listener.close(() => {
        if (typeof address === "object" && address) resolve(address.port);
        else reject(new Error("no available port"));
      });
    });
  });
}

async function main(): Promise<void> {
  const port = await freePort();
  const machine = await Machine.create(
    {
      name: `sdk-agent-ready-${process.pid}`,
      image: "node:24-alpine",
      ports: [{ host: port, guest: 80 }],
      waitForPorts: false,
      network: true,
    },
    { target: "local", handleSignals: false },
  );
  let deleted = false;
  try {
    assert.equal(await machine.ready(), false, "port is not yet serving after create");
    const result = await machine.exec([
      "sh", "-c",
      `nohup node -e "require('http').createServer((_, res) => res.end('ok')).listen(80, '0.0.0.0')" >/tmp/server.log 2>&1 & echo $! >/tmp/server.pid`,
    ]);
    result.assertSuccess();
    await machine.waitUntilReady({ timeoutMs: 15_000, intervalMs: 100 });
    assert.equal(await (await machine.fetch(80)).text(), "ok");
    (await machine.exec(["sh", "-c", "kill $(cat /tmp/server.pid)"])).assertSuccess();
    assert.equal(await machine.ready(), false, "the listener stopped");
    const attached = await Machine.connect(machine.name, {
      target: "local", handleSignals: false, waitForPorts: false,
    });
    assert.equal(await attached.ready(), false, "connect returned without its port");
    await attached.delete();
    deleted = true;
    console.log("local create and connect returned with agent ready; both worked without a published listener");
  } finally {
    if (!deleted) await machine.delete();
  }
}

void main().catch((error: unknown) => { console.error(error); process.exitCode = 1; });
