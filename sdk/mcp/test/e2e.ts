/**
 * End-to-end test for smolmachines-mcp against the real local engine.
 *
 * Spawns the built server over stdio, drives it with the MCP client, and walks
 * the branch-try-keep flow an agent would:
 *   create (with workspace) → exec → branch → change in the branch →
 *   source untouched → export branch results → delete branch →
 *   checkpoint → restore → list → delete; then a machine created with
 *   `persistent: false` is gone once the client disconnects.
 *
 *   cd smol/sdk/mcp && npm install && npm run test:e2e
 *
 * Requires a host the engine supports (macOS Apple Silicon, or Linux + KVM).
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { mkdtemp, writeFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

let passed = 0;
let failed = 0;
function check(label: string, ok: boolean, detail = ""): void {
  if (ok) {
    passed++;
    console.log(`  ✓ ${label}`);
  } else {
    failed++;
    console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

type ToolResult = { content: Array<{ type: string; text?: string }>; isError?: boolean };

async function main(): Promise<void> {
  console.log("smolmachines-mcp e2e\n");
  const connect = async () => {
    const client = new Client({ name: "e2e", version: "0" });
    await client.connect(
      new StdioClientTransport({
        command: process.execPath,
        args: [path.resolve("dist/index.js")],
        env: { ...process.env, SMOL_TARGET: "local" } as Record<string, string>,
        stderr: "inherit",
      }),
    );
    return client;
  };
  let client = await connect();

  const call = async (name: string, args: Record<string, unknown> = {}) => {
    const result = (await client.callTool({ name, arguments: args })) as ToolResult;
    const body = result.content.find((c) => c.type === "text")?.text ?? "";
    let json: any = null;
    try {
      json = JSON.parse(body);
    } catch {
      /* plain text */
    }
    return { result, body, json };
  };

  const tools = (await client.listTools()).tools.map((t) => t.name).sort();
  check(
    "tools registered",
    ["smol_branch", "smol_checkpoint", "smol_create", "smol_delete", "smol_exec", "smol_export", "smol_list", "smol_read_file", "smol_restore", "smol_write_file"].every((t) => tools.includes(t)),
    tools.join(","),
  );

  const work = await mkdtemp(path.join(tmpdir(), "smol-mcp-ws-"));
  const out = await mkdtemp(path.join(tmpdir(), "smol-mcp-out-"));
  await writeFile(path.join(work, "hello.txt"), "from the host\n");
  await writeFile(path.join(work, "junk.log"), "x".repeat(1000));
  const stamp = Date.now().toString(36);
  const src = `mcp-e2e-${stamp}`;
  const created: string[] = [];

  try {
    const t0 = Date.now();
    const create = await call("smol_create", {
      name: src,
      image: "alpine:3.20",
      workspace: work,
      exclude: ["*.log"],
      memoryMb: 1024,
    });
    check("create succeeds", !create.result.isError && create.json?.machine === src, create.body.slice(0, 300));
    created.push(src);
    console.log(`  (create + workspace copy in ${Date.now() - t0}ms)`);

    const ws = await call("smol_exec", { machine: src, command: "cat hello.txt && ls" });
    check("workspace copied in, default workdir /workspace", ws.json?.exitCode === 0 && ws.json?.stdout.includes("from the host"), ws.body.slice(0, 300));
    check("exclude pattern honored", !ws.json?.stdout.includes("junk.log"), ws.json?.stdout);

    const os = await call("smol_exec", { machine: src, command: "cat /etc/os-release | head -1" });
    check("image is the machine rootfs", os.json?.stdout.toLowerCase().includes("alpine"), os.json?.stdout);

    const fail = await call("smol_exec", { machine: src, command: "exit 3" });
    check("non-zero exit reported as error with code", fail.result.isError === true && fail.json?.exitCode === 3, fail.body);

    await call("smol_exec", { machine: src, command: "dd if=/dev/urandom of=/workspace/data.bin bs=1M count=20 2>/dev/null; sha256sum data.bin | cut -c1-16 > data.sha" });
    const srcSha = (await call("smol_exec", { machine: src, command: "cat data.sha" })).json?.stdout.trim();

    const t1 = Date.now();
    const branch = await call("smol_branch", { source: src });
    const child = branch.json?.machine as string;
    check("branch succeeds", !branch.result.isError && typeof child === "string", branch.body.slice(0, 300));
    created.push(child);
    console.log(`  (branch in ${Date.now() - t1}ms, server-measured ${branch.json?.ms}ms)`);

    const childSha = (await call("smol_exec", { machine: child, command: "sha256sum data.bin | cut -c1-16" })).json?.stdout.trim();
    check("child inherits source state", childSha === srcSha, `${childSha} vs ${srcSha}`);

    await call("smol_exec", { machine: child, command: "echo changed-in-branch > hello.txt && rm data.bin" });
    const childHello = (await call("smol_read_file", { machine: child, path: "/workspace/hello.txt" })).body.trim();
    const srcHello = (await call("smol_read_file", { machine: src, path: "/workspace/hello.txt" })).body.trim();
    check("branch change visible in child", childHello === "changed-in-branch", childHello);
    check("source untouched by child", srcHello === "from the host", srcHello);
    check("source still has data.bin", (await call("smol_exec", { machine: src, command: "test -f data.bin" })).json?.exitCode === 0);

    const grand = await call("smol_branch", { source: child, name: `${src}-gc` });
    check("child is itself branchable (depth 2)", !grand.result.isError, grand.body.slice(0, 200));
    created.push(`${src}-gc`);
    const grandHello = (await call("smol_read_file", { machine: `${src}-gc`, path: "/workspace/hello.txt" })).body.trim();
    check("grandchild has the child's state", grandHello === "changed-in-branch", grandHello);

    const write = await call("smol_write_file", { machine: child, path: "/workspace/out/result.txt", content: "kept\n" });
    check("write_file creates parents", !write.result.isError, write.body);
    const exp = await call("smol_export", { machine: child, guestPath: "/workspace", hostPath: out });
    check("export succeeds", !exp.result.isError && exp.json?.bytes > 0, exp.body.slice(0, 300));
    const exported = await readFile(path.join(out, "hello.txt"), "utf8").catch(() => "");
    const kept = await readFile(path.join(out, "out", "result.txt"), "utf8").catch(() => "");
    check("exported files match the branch", exported.trim() === "changed-in-branch" && kept.trim() === "kept", `${exported}|${kept}`);
    const hostHello = await readFile(path.join(work, "hello.txt"), "utf8");
    check("original host workspace untouched", hostHello.trim() === "from the host", hostHello);

    const del = await call("smol_delete", { machine: `${src}-gc` });
    check("delete grandchild", !del.result.isError, del.body);
    const del2 = await call("smol_delete", { machine: child });
    check("discard branch", !del2.result.isError, del2.body);
    created.splice(created.indexOf(child), 1);
    created.splice(created.indexOf(`${src}-gc`), 1);

    const t2 = Date.now();
    const cp = await call("smol_checkpoint", { machine: src });
    check("checkpoint succeeds", !cp.result.isError && typeof cp.json?.checkpoint === "string", cp.body.slice(0, 300));
    console.log(`  (checkpoint in ${Date.now() - t2}ms, ${cp.json?.sizeBytes} bytes)`);
    await call("smol_exec", { machine: src, command: "echo after-checkpoint > hello.txt" });

    const t3 = Date.now();
    const rs = await call("smol_restore", { checkpoint: cp.json?.checkpoint, name: `${src}-r` });
    check("restore succeeds", !rs.result.isError && rs.json?.machine === `${src}-r`, rs.body.slice(0, 300));
    created.push(`${src}-r`);
    console.log(`  (restore in ${Date.now() - t3}ms)`);
    const restoredHello = (await call("smol_read_file", { machine: `${src}-r`, path: "/workspace/hello.txt" })).body.trim();
    check("restored machine is the saved state (rewind)", restoredHello === "from the host", restoredHello);
    const restoredBranch = await call("smol_branch", { source: `${src}-r` });
    check("restored machine is branchable", !restoredBranch.result.isError, restoredBranch.body.slice(0, 200));
    if (restoredBranch.json?.machine) created.push(restoredBranch.json.machine);
    if (cp.json?.checkpoint) await rm(cp.json.checkpoint, { recursive: true, force: true });

    const list = await call("smol_list");
    check("list includes our machines", Array.isArray(list.json) && list.json.some((m: any) => m.machine === src), list.body.slice(0, 200));

    await call("smol_create", { name: `${src}-eph`, persistent: false, memoryMb: 512 });
    await call("smol_branch", { source: `${src}-eph`, name: `${src}-eph-b` });
  } finally {
    for (const name of [...created].reverse()) {
      await call("smol_delete", { machine: name }).catch(() => undefined);
    }
    await rm(work, { recursive: true, force: true });
    await rm(out, { recursive: true, force: true });
    await client.close();
  }

  // A client that goes away must not leave behind what it did not ask to keep.
  client = await connect();
  const left = (await call("smol_list")).json?.filter((m: any) => m.machine.startsWith(`${src}-eph`)) ?? [];
  check("disconnect deletes non-persistent machines and their branches", left.length === 0, JSON.stringify(left));
  for (const m of left.reverse()) await call("smol_delete", { machine: m.machine });
  await client.close();

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
