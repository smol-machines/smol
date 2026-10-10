/** Run with `npm exec -- tsx test/local-paused-connect.ts` after building the native addon. */
import assert from "node:assert/strict";
import { Machine } from "../machine";

async function main(): Promise<void> {
  const name = `sdk-paused-connect-${process.pid}-${Date.now()}`;
  const options = { target: "local", handleSignals: false } as const;
  const machine = await Machine.create({
    name,
    detach: true,
    network: false,
  }, options);
  try {
    (await machine.exec(["sh", "-c", "echo persisted >/tmp/paused-connect.txt"])).assertSuccess();
    await machine.pause();
    assert.equal(await machine.state(), "paused");

    // A second SDK object must attach without starting a new VM over the saved RAM.
    const attached = await Machine.connect(name, options);
    assert.equal(await attached.state(), "paused");
    assert.equal(await attached.ready(), false);
    await assert.rejects(
      Machine.connect(name, {
        ...options,
        egressInterceptor: { address: "127.0.0.1:12345", token: "a5".repeat(32) },
      }),
      /cannot bind an egress interceptor to a paused machine/,
    );
    assert.equal(await attached.state(), "paused");
    await attached.resume();
    assert.equal(await attached.state(), "running");
    assert.equal((await attached.exec(["cat", "/tmp/paused-connect.txt"])).stdout.trim(), "persisted");
    console.log("paused connect, explicit resume, and saved guest state passed");
  } finally {
    await machine.delete();
  }
}

void main().catch((error: unknown) => { console.error(error); process.exitCode = 1; });
