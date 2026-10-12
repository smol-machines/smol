/** A durable eve session must survive the death of the process that started it. */
import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Machine } from "../index";
import { SmolmachinesSandbox } from "../eve";

type Session = {
  storagePath: string;
  id: string;
  artifact: any;
  state: { machine: string };
};

const [phase, stateFile] = process.argv.slice(2);
const environment = SmolmachinesSandbox.environment();
const impl = (environment as any)[Symbol.for("eve.sandbox-provider-runtime")].implementation;
const context = (storagePath: string, id: string) => ({ host: {}, session: { id }, storagePath });

async function run() {
  if (phase === "init") {
    const storagePath = mkdtempSync(join(tmpdir(), "eve-smol-process-resume-"));
    const artifact = await impl.prepare({
      files: { list: async () => [], read: async () => new Uint8Array(), readText: async () => "" },
      host: {},
      resources: { source: { kind: "none" } },
      sourceRevision: "process-resume-e2e",
      storagePath,
    });
    const id = `eve-resume-${process.pid}`;
    const { handle, state } = await impl.start(context(storagePath, id), { networkPolicy: "deny-all" }, artifact);
    try {
      await handle.sandbox.writeTextFile({ path: "proof.txt", content: "durable session\n" });
    } finally {
      await handle.onSandboxStop();
    }
    writeFileSync(stateFile, JSON.stringify({ storagePath, id, artifact, state } satisfies Session));
    console.log(`Stopped session machine ${state.machine} for a different process to resume`);
  } else if (phase === "resume") {
    const { storagePath, id, artifact, state } = JSON.parse(readFileSync(stateFile, "utf8")) as Session;
    try {
      const handle = await impl.resume(context(storagePath, id), artifact, state);
      assert.equal(await handle.sandbox.readTextFile({ path: "proof.txt" }), "durable session\n");
      const network = await handle.sandbox.run({
        command: "curl -s -o /dev/null -m 5 -w '%{http_code}' https://example.com || echo blocked",
      });
      assert.match(network.stdout, /blocked/);
      await handle.onSandboxStop();
      await impl.onSessionEnd(context(storagePath, id), artifact, state, { reason: "completed" });
      assert.equal((await Machine.list({ target: "local" })).some((machine) => machine.name === state.machine), false);
      console.log("Resumed across process restart with disk and network policy intact; reclaimed the VM");
    } finally {
      rmSync(storagePath, { recursive: true, force: true });
    }
  } else {
    throw new Error("Specify init or resume and a state file");
  }
}

run().catch((error) => {
  console.error("EVE PROCESS RESUME FAILED:", error);
  process.exitCode = 1;
});
