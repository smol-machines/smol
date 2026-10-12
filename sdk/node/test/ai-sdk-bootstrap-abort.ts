/** An abort while the workspace is created must prevent any workload launch. */
import assert from "node:assert/strict";
import type { Machine } from "../machine";
import { createSandboxSession } from "../ai-sdk";

async function abortedBeforeCommand(mode: "run" | "spawn"): Promise<void> {
  let workspaceStarted!: () => void;
  let finishWorkspace!: () => void;
  const started = new Promise<void>((resolve) => (workspaceStarted = resolve));
  const workspace = new Promise<void>((resolve) => (finishWorkspace = resolve));
  let commandLaunched = false;
  const machine = {
    async exec(command: string[]) {
      if (command[0] !== "mkdir") {
        commandLaunched = true;
        throw new Error("command launched after cancellation");
      }
      workspaceStarted();
      await workspace;
      return { exitCode: 0, stderr: "" };
    },
    execStream() {
      commandLaunched = true;
      throw new Error("command launched after cancellation");
    },
  } as unknown as Machine;

  const controller = new AbortController();
  const session = createSandboxSession(machine);
  const operation = session[mode]({ command: "echo must-not-run", abortSignal: controller.signal });
  const rejection = assert.rejects(operation, /cancelled while preparing workspace/);
  await started;
  controller.abort(new Error("cancelled while preparing workspace"));
  finishWorkspace();
  await rejection;
  assert.equal(commandLaunched, false, `${mode} launched after cancellation`);
}

async function main(): Promise<void> {
  await abortedBeforeCommand("run");
  await abortedBeforeCommand("spawn");
  console.log("AI SDK workspace cancellation: run and spawn passed");
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});
