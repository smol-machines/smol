/** End-to-end test against a real local microVM and Sandcastle's published API. */
import { strict as assert } from "node:assert";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSandbox, type AgentProvider } from "@ai-hero/sandcastle";
import { smol } from "./provider.js";

const gitHosts = [
  "registry-1.docker.io", "auth.docker.io", "production.cloudflare.docker.com", "index.docker.io",
];
const host = await mkdtemp(join(tmpdir(), "sandcastle-smol-live-"));
const git = (...args: string[]) => execFileSync("git", args, { cwd: host, stdio: "pipe" });

// Scripted agent exercises the real Sandcastle lifecycle without an API key.
const agent: AgentProvider = {
  name: "scripted",
  env: {},
  captureSessions: false,
  buildPrintCommand: () => ({
    command: "printf 'after\\n' > tracked.txt; printf 'untracked\\n' > created.txt; echo '<promise>COMPLETE</promise>'",
  }),
  parseStreamLine: (line) => [{ type: "text", text: line }],
};

try {
  git("init", "-b", "main");
  git("config", "user.name", "Example");
  git("config", "user.email", "example@example.com");
  await writeFile(join(host, "tracked.txt"), "before\n");
  git("add", "tracked.txt");
  git("commit", "-m", "Initial commit");

  console.log("Creating Sandcastle sandbox in a local Smol VM");
  const sandbox = await createSandbox({
    branch: "smol-sandcastle-live",
    cwd: host,
    sandbox: smol({ image: "alpine/git:2.47.2", allowHosts: gitHosts, cpus: 1, memoryMb: 1024 }),
  });
  try {
    assert.equal((await sandbox.exec("cat tracked.txt")).stdout, "before\n");
    const lines: string[] = [];
    const stdin = await sandbox.exec("wc -c", {
      stdin: "x".repeat(180_000),
      onLine: (line) => lines.push(line),
    });
    assert.equal(stdin.stdout.trim(), "180000");
    assert.deepEqual(lines, ["180000"]);
    const result = await sandbox.run({ agent, prompt: "Edit the repo", maxIterations: 1 });
    assert.equal(result.completionSignal, "<promise>COMPLETE</promise>");
    assert.equal(await readFile(join(sandbox.worktreePath, "tracked.txt"), "utf8"), "after\n");
    assert.equal(await readFile(join(sandbox.worktreePath, "created.txt"), "utf8"), "untracked\n");
    console.log("PASS: real Sandcastle run synced tracked and untracked changes from a local VM");
  } finally {
    await sandbox.close();
  }
} finally {
  await rm(host, { recursive: true, force: true });
}
