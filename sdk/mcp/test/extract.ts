/**
 * Tests for the untrusted-archive extractor behind smol_export. No engine
 * needed: archives are built here the way a malicious machine could build them.
 *
 *   cd smol/sdk/mcp && npm install && npm run test:extract
 */

import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { lstat, mkdir, mkdtemp, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { gzipSync } from "node:zlib";
import { extractUntrusted } from "../src/extract.js";

const MAX = 64 * 1024 * 1024;
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

interface Member {
  name: string;
  type?: "0" | "1" | "2" | "5" | "3";
  data?: string;
  link?: string;
  mode?: number;
}

function header(name: string, type: string, size: number, mode: number, link = ""): Buffer {
  const h = Buffer.alloc(512);
  h.write(name, 0, 100, "utf8");
  h.write(`${mode.toString(8).padStart(7, "0")}\0`, 100);
  h.write("0000000\0", 108);
  h.write("0000000\0", 116);
  h.write(`${size.toString(8).padStart(11, "0")}\0`, 124);
  h.write("00000000000\0", 136);
  h.write("        ", 148);
  h.write(type, 156);
  h.write(link, 157, 100, "utf8");
  h.write("ustar\0", 257);
  h.write("00", 263);
  let sum = 0;
  for (const byte of h) sum += byte;
  h.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148);
  return h;
}

/** A tar.gz with exactly these members, as a machine could produce it. */
function archive(members: Member[]): Buffer {
  const parts: Buffer[] = [];
  for (const m of members) {
    const type = m.type ?? "0";
    const data = Buffer.from(type === "0" ? (m.data ?? "") : "");
    parts.push(header(m.name, type, data.length, m.mode ?? (type === "5" ? 0o755 : 0o644), m.link));
    parts.push(data);
    const pad = (512 - (data.length % 512)) % 512;
    parts.push(Buffer.alloc(pad));
  }
  parts.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(parts));
}

async function rejects(fn: () => Promise<unknown>, pattern: RegExp): Promise<string> {
  try {
    await fn();
    return "resolved";
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return pattern.test(message) ? "" : message;
  }
}

async function main(): Promise<void> {
  const work = await mkdtemp(path.join(tmpdir(), "smol-extract-"));
  try {
    const outside = path.join(work, "outside");
    await mkdir(outside);

    console.log("plant-then-write across two exports (the reported escape)");
    const dest = path.join(work, "dest");
    await mkdir(dest);
    const first = await extractUntrusted(archive([{ name: "link", type: "2", link: outside }]), dest, MAX);
    check("the symlink is not created", !existsSync(path.join(dest, "link")));
    check("the symlink is reported as skipped", first.skipped.includes("link"));
    const second = await rejects(
      () => extractUntrusted(archive([{ name: "link/canary.txt", data: "escaped" }]), dest, MAX),
      /./,
    );
    check("nothing is written outside hostPath", !existsSync(path.join(outside, "canary.txt")), second);
    check("the second export lands inside hostPath", existsSync(path.join(dest, "link", "canary.txt")));

    console.log("a symlink already in hostPath is never written through");
    const planted = path.join(work, "planted");
    await mkdir(planted);
    await symlink(outside, path.join(planted, "link"));
    const through = await rejects(
      () => extractUntrusted(archive([{ name: "link/canary2.txt", data: "escaped" }]), planted, MAX),
      /symlink/,
    );
    check("refused with a symlink error", through === "", through);
    check("no file appears outside", !existsSync(path.join(outside, "canary2.txt")));
    await symlink(path.join(outside, "target.txt"), path.join(planted, "file.txt"));
    const leaf = await rejects(
      () => extractUntrusted(archive([{ name: "file.txt", data: "escaped" }]), planted, MAX),
      /symlink/,
    );
    check("a symlinked file is not followed", leaf === "" && !existsSync(path.join(outside, "target.txt")), leaf);

    console.log("unsafe member names are refused");
    for (const name of ["../escape.txt", "a/../../escape.txt", "/tmp/abs.txt"]) {
      const d = await mkdtemp(path.join(work, "names-"));
      const why = await rejects(() => extractUntrusted(archive([{ name, data: "x" }]), d, MAX), /relative|'\.\.'/);
      check(`refuses ${name}`, why === "", why);
    }
    check("nothing escaped next to the test dirs", !existsSync(path.join(work, "escape.txt")));

    console.log("hardlinks and devices are skipped");
    const d2 = path.join(work, "links");
    await mkdir(d2);
    const linked = await extractUntrusted(
      archive([
        { name: "hard", type: "1", link: "/etc/passwd" },
        { name: "dev", type: "3" },
        { name: "ok.txt", data: "fine" },
      ]),
      d2,
      MAX,
    );
    check("only the regular file is written", linked.files === 1 && !existsSync(path.join(d2, "hard")) && !existsSync(path.join(d2, "dev")));
    check("both are reported", linked.skipped.includes("hard") && linked.skipped.includes("dev"));

    console.log("modes keep only permission bits");
    const d3 = path.join(work, "modes");
    await mkdir(d3);
    await extractUntrusted(archive([{ name: "suid", data: "#!/bin/sh\n", mode: 0o4755 }]), d3, MAX);
    const mode = (await stat(path.join(d3, "suid"))).mode & 0o7777;
    check("setuid is stripped, exec kept", mode === 0o755, mode.toString(8));
    await extractUntrusted(archive([{ name: "shared", data: "x", mode: 0o777 }]), d3, MAX);
    const shared = (await stat(path.join(d3, "shared"))).mode & 0o7777;
    check("group and world write are dropped", shared === 0o755, shared.toString(8));

    console.log("decompression is bounded");
    const bomb = await rejects(
      () => extractUntrusted(archive([{ name: "big", data: "a".repeat(4096) }]), d3, 1024),
      /expands past/,
    );
    check("an archive that expands past the cap is refused", bomb === "", bomb);

    console.log("ordinary exports still work");
    const src = path.join(work, "src");
    const deep = path.join(src, "a".repeat(60), "b".repeat(60));
    await mkdir(deep, { recursive: true });
    await writeFile(path.join(deep, "nested.txt"), "deep");
    await writeFile(path.join(src, "top.txt"), "top");
    await mkdir(path.join(src, "empty"));
    await symlink("top.txt", path.join(src, "alias"));
    const real = execFileSync("tar", ["-czf", "-", "-C", src, "."]);
    const out = path.join(work, "out");
    await mkdir(out);
    await writeFile(path.join(out, "top.txt"), "old contents");
    const done = await extractUntrusted(real, out, MAX);
    check("files written", (await readFile(path.join(out, "top.txt"), "utf8")) === "top", "overwrite of existing file");
    check(
      "names past 100 bytes survive",
      (await readFile(path.join(out, "a".repeat(60), "b".repeat(60), "nested.txt"), "utf8")) === "deep",
    );
    check("empty directories survive", (await lstat(path.join(out, "empty"))).isDirectory());
    check("the symlink from the host's own tar is skipped, not followed", !existsSync(path.join(out, "alias")) && done.skipped.some((s) => s.endsWith("alias")));
  } finally {
    await rm(work, { recursive: true, force: true });
  }
  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed) process.exit(1);
}

await main();
