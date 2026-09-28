/**
 * The AI SDK sandbox session on a real local machine: commands, streaming,
 * kill and abort, and the file API. Needs KVM (or Apple Silicon) and outbound
 * network for the image pull.
 */
import { Machine } from "../index";
import { createSandboxSession } from "../ai-sdk";

const opts = { target: "local" as const, handleSignals: false };
let failures = 0;
function check(label: string, ok: boolean, detail = "") {
  if (!ok) failures++;
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  (${detail})` : ""}`);
}
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
async function rejects(p: PromiseLike<unknown>): Promise<string> {
  try {
    await p;
    return "";
  } catch (e) {
    return (e as Error).name || "Error";
  }
}
async function readAll(stream: ReadableStream<Uint8Array>): Promise<string> {
  let out = "";
  const decoder = new TextDecoder();
  for await (const chunk of stream as unknown as AsyncIterable<Uint8Array>) out += decoder.decode(chunk, { stream: true });
  return out;
}

async function main() {
  const machine = await Machine.create({ name: `aisdk-${process.pid}`, image: "alpine", network: true }, opts);
  try {
    const sandbox = createSandboxSession(machine);
    check("has a description for the model", sandbox.description.includes("/workspace"));

    // run
    const r = await sandbox.run({ command: "echo out; echo err >&2; exit 3" });
    check("run returns stdout, stderr and the exit code", r.stdout === "out\n" && r.stderr === "err\n" && r.exitCode === 3, JSON.stringify(r));
    const cwd = await sandbox.run({ command: "pwd" });
    check("the working directory defaults to /workspace", cwd.stdout.trim() === "/workspace", cwd.stdout.trim());
    await sandbox.run({ command: "mkdir -p sub" });
    const sub = await sandbox.run({ command: "pwd", workingDirectory: "sub" });
    check("a relative working directory resolves under /workspace", sub.stdout.trim() === "/workspace/sub", sub.stdout.trim());
    const env = await sandbox.run({ command: 'echo "$GREETING"', env: { GREETING: "hi there" } });
    check("env reaches the command", env.stdout.trim() === "hi there", env.stdout.trim());
    const pipe = await sandbox.run({ command: "printf 'b\\na\\n' | sort | tr -d '\\n'" });
    check("shell syntax works", pipe.stdout === "ab", pipe.stdout);

    const ac = new AbortController();
    const started = Date.now();
    setTimeout(() => ac.abort(), 500);
    const aborted = await rejects(sandbox.run({ command: "sleep 30", abortSignal: ac.signal }));
    check("an aborted run rejects promptly", aborted !== "" && Date.now() - started < 5000, `${aborted} after ${Date.now() - started} ms`);

    // spawn
    const t0 = Date.now();
    const proc = await sandbox.spawn({ command: "echo first; sleep 1; echo second; echo oops >&2; exit 7" });
    const reader = proc.stdout.getReader();
    const firstChunk = new TextDecoder().decode((await reader.read()).value);
    check("spawn streams output before the process exits", firstChunk.startsWith("first") && Date.now() - t0 < 900, `${JSON.stringify(firstChunk)} at ${Date.now() - t0} ms`);
    let rest = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      rest += new TextDecoder().decode(value);
    }
    const [err, exit] = await Promise.all([readAll(proc.stderr), proc.wait()]);
    check("spawn delivers the rest, stderr and the exit code", rest.includes("second") && err.trim() === "oops" && exit.exitCode === 7, `${JSON.stringify(rest)} ${JSON.stringify(err)} ${exit.exitCode}`);

    const beat = await sandbox.spawn({ command: "i=0; while true; do i=$((i+1)); echo $i > beat; sleep 0.2; done" });
    await sleep(1000);
    const a = (await sandbox.readTextFile({ path: "beat" }))?.trim();
    await beat.kill();
    await beat.kill(); // idempotent
    const code = (await beat.wait()).exitCode;
    await sleep(600);
    const b = (await sandbox.readTextFile({ path: "beat" }))?.trim();
    await sleep(600);
    const c = (await sandbox.readTextFile({ path: "beat" }))?.trim();
    check("kill stops the process and wait reports it", a !== undefined && code === 137 && b === c, `before ${a}, after ${b} then ${c}, exit ${code}`);

    const ac2 = new AbortController();
    const spun = await sandbox.spawn({ command: "sleep 30", abortSignal: ac2.signal });
    setTimeout(() => ac2.abort(), 300);
    check("an aborted spawn's wait rejects", (await rejects(spun.wait())) !== "");

    // files
    await sandbox.writeTextFile({ path: "deep/nested/notes.txt", content: "one\ntwo\nthree\nfour" });
    check("writeTextFile creates parent directories", (await sandbox.run({ command: "cat deep/nested/notes.txt" })).stdout === "one\ntwo\nthree\nfour");
    check("readTextFile line ranges are 1-based and inclusive", (await sandbox.readTextFile({ path: "deep/nested/notes.txt", startLine: 2, endLine: 3 })) === "two\nthree");
    check("an endLine past EOF reads through EOF", (await sandbox.readTextFile({ path: "deep/nested/notes.txt", startLine: 4, endLine: 99 })) === "four");
    check("a missing file reads as null", (await sandbox.readTextFile({ path: "nope.txt" })) === null && (await sandbox.readBinaryFile({ path: "/nope/at/all" })) === null);

    const bytes = new Uint8Array(256).map((_, i) => i);
    await sandbox.writeBinaryFile({ path: "/tmp/all-bytes.bin", content: bytes });
    const back = await sandbox.readBinaryFile({ path: "/tmp/all-bytes.bin" });
    check("binary files round-trip byte for byte (absolute path)", back !== null && back.length === 256 && back.every((v, i) => v === i));

    await sandbox.writeFile({
      path: "streamed.txt",
      content: new ReadableStream({
        start(ctl) {
          ctl.enqueue(new TextEncoder().encode("from "));
          ctl.enqueue(new TextEncoder().encode("a stream"));
          ctl.close();
        },
      }),
    });
    const streamed = await sandbox.readFile({ path: "streamed.txt" });
    check("writeFile and readFile move streams", streamed !== null && (await readAll(streamed)) === "from a stream");

    await sandbox.removePath({ path: "deep", recursive: true });
    check("removePath removes a directory tree", (await sandbox.readTextFile({ path: "deep/nested/notes.txt" })) === null);
    await sandbox.removePath({ path: "not-there", force: true });
    check("removePath with force ignores a missing path", true);

    const asUser = createSandboxSession(machine, { user: "nobody" });
    check("a session can run every command as another user", (await asUser.run({ command: "id -u" })).stdout.trim() === "65534");
  } finally {
    await machine.delete().catch(() => {});
  }
  console.log(`\n${failures === 0 ? "ALL PASSED" : `${failures} FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("AI SDK E2E CRASHED:", e);
  process.exit(2);
});
