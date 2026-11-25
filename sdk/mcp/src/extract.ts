/**
 * Unpack a machine-produced tar.gz onto the host without trusting it.
 *
 * The archive comes from inside a machine, so its contents are untrusted. Only
 * regular files and directories are written; links, devices and FIFOs are
 * skipped and reported. Every member must be a relative path with no `..`,
 * and no write follows a symlink: each path component under the destination is
 * checked with lstat, and files are opened with O_NOFOLLOW. Ownership is never
 * applied, and modes keep only permission bits without group or world write
 * (at most 0755): setuid/setgid/sticky never reach the host, and a machine
 * cannot leave a file there that other local users can change.
 */

import { constants } from "node:fs";
import { lstat, mkdir, open, realpath } from "node:fs/promises";
import path from "node:path";
import { gunzipSync } from "node:zlib";

export interface ExtractResult {
  files: number;
  directories: number;
  /** Members not written: links, devices, FIFOs. */
  skipped: string[];
}

const BLOCK = 512;
/** Skipped names reported back; the count is still exact. */
const SKIPPED_REPORT_CAP = 50;

function field(header: Buffer, offset: number, length: number): string {
  const raw = header.subarray(offset, offset + length);
  const end = raw.indexOf(0);
  return raw.subarray(0, end === -1 ? raw.length : end).toString("utf8");
}

function octal(header: Buffer, offset: number, length: number, what: string): number {
  if (header[offset] & 0x80) throw new Error(`archive ${what} uses base-256, which is not supported`);
  const text = field(header, offset, length).trim();
  if (text === "") return 0;
  if (!/^[0-7]+$/.test(text)) throw new Error(`archive ${what} is not octal: ${JSON.stringify(text)}`);
  return parseInt(text, 8);
}

function checksumOk(header: Buffer): boolean {
  const stored = octal(header, 148, 8, "checksum");
  // Writers differ on signed versus unsigned bytes; accept either, as tar does.
  let unsigned = 0;
  let signed = 0;
  for (let i = 0; i < BLOCK; i++) {
    const byte = i >= 148 && i < 156 ? 0x20 : header[i];
    unsigned += byte;
    signed += byte > 127 ? byte - 256 : byte;
  }
  return stored === unsigned || stored === signed;
}

/** Values of a pax extended header record set (`<len> key=value\n`...). */
function paxRecords(data: Buffer): Map<string, string> {
  const records = new Map<string, string>();
  let pos = 0;
  while (pos < data.length) {
    const space = data.indexOf(0x20, pos);
    if (space === -1) break;
    const length = parseInt(data.subarray(pos, space).toString("ascii"), 10);
    if (!Number.isFinite(length) || length <= 0 || pos + length > data.length) {
      throw new Error("archive has a malformed pax header");
    }
    const record = data.subarray(space + 1, pos + length - 1).toString("utf8");
    const eq = record.indexOf("=");
    if (eq > 0) records.set(record.slice(0, eq), record.slice(eq + 1));
    pos += length;
  }
  return records;
}

/** The member's path segments, or an error naming why it is refused. */
export function memberSegments(name: string): string[] {
  if (name.includes("\0")) throw new Error(`archive member name contains NUL: ${JSON.stringify(name)}`);
  if (name.startsWith("/") || /^[A-Za-z]:/.test(name) || name.includes("\\")) {
    throw new Error(`archive member is not a relative path: ${JSON.stringify(name)}`);
  }
  const segments = name.split("/").filter((s) => s !== "" && s !== ".");
  if (segments.includes("..")) throw new Error(`archive member contains '..': ${JSON.stringify(name)}`);
  return segments;
}

/**
 * Walk `segments` under `root`, creating missing directories, and refuse any
 * component that exists as a symlink or a non-directory.
 */
async function ensureDirectory(root: string, segments: string[]): Promise<string> {
  let current = root;
  for (const segment of segments) {
    current = path.join(current, segment);
    let info;
    try {
      info = await lstat(current);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      await mkdir(current, { mode: 0o755 });
      continue;
    }
    if (info.isSymbolicLink()) throw new Error(`refusing to write through a symlink at ${current}`);
    if (!info.isDirectory()) throw new Error(`refusing to replace non-directory ${current} with a directory`);
  }
  return current;
}

async function writeFileMember(root: string, segments: string[], data: Buffer, mode: number): Promise<void> {
  const dir = await ensureDirectory(root, segments.slice(0, -1));
  const target = path.join(dir, segments[segments.length - 1]);
  try {
    const info = await lstat(target);
    if (info.isSymbolicLink()) throw new Error(`refusing to write through a symlink at ${target}`);
    if (!info.isFile()) throw new Error(`refusing to replace non-file ${target}`);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }
  const flags = constants.O_WRONLY | constants.O_CREAT | constants.O_TRUNC | (constants.O_NOFOLLOW ?? 0);
  // At most 0755: no setuid/setgid/sticky, and no group or world write, which
  // would let any other local user rewrite what the machine exported.
  const permissions = mode & 0o755;
  const handle = await open(target, flags, permissions);
  try {
    await handle.writeFile(data);
    await handle.chmod(permissions);
  } finally {
    await handle.close();
  }
}

/**
 * Extract a gzip-compressed tar archive into `hostPath` (an existing
 * directory). `maxBytes` bounds the decompressed size, so a small archive
 * cannot expand without limit.
 */
export async function extractUntrusted(archive: Buffer, hostPath: string, maxBytes: number): Promise<ExtractResult> {
  const root = await realpath(hostPath);
  let tar: Buffer;
  try {
    tar = gunzipSync(archive, { maxOutputLength: maxBytes });
  } catch (error) {
    if (error instanceof RangeError) throw new Error(`export expands past ${maxBytes} bytes`);
    throw error;
  }
  const result: ExtractResult = { files: 0, directories: 0, skipped: [] };
  let skippedCount = 0;
  let longName: string | undefined;
  let paxPath: string | undefined;
  let pos = 0;

  while (pos + BLOCK <= tar.length) {
    const header = tar.subarray(pos, pos + BLOCK);
    pos += BLOCK;
    if (header.every((byte) => byte === 0)) break;
    if (!checksumOk(header)) throw new Error("archive header checksum mismatch");

    const size = octal(header, 124, 12, "size");
    const type = String.fromCharCode(header[156] || 0x30);
    const end = pos + size;
    if (end > tar.length) throw new Error("archive is truncated");
    const data = tar.subarray(pos, end);
    pos += Math.ceil(size / BLOCK) * BLOCK;

    // Headers that only describe the next member.
    if (type === "L") {
      longName = field(data, 0, data.length);
      continue;
    }
    if (type === "x") {
      paxPath = paxRecords(data).get("path") ?? paxPath;
      continue;
    }
    if (type === "g" || type === "K") continue;

    const prefix = field(header, 345, 155);
    const shortName = field(header, 0, 100);
    const name = paxPath ?? longName ?? (prefix ? `${prefix}/${shortName}` : shortName);
    longName = undefined;
    paxPath = undefined;

    const segments = memberSegments(name);
    if (type === "5") {
      if (segments.length) await ensureDirectory(root, segments);
      result.directories++;
    } else if (type === "0" || type === "7") {
      if (!segments.length) throw new Error(`archive file member has no name: ${JSON.stringify(name)}`);
      await writeFileMember(root, segments, data, octal(header, 100, 8, "mode"));
      result.files++;
    } else {
      // Symlinks, hardlinks, devices, FIFOs: never created on the host.
      if (skippedCount++ < SKIPPED_REPORT_CAP) result.skipped.push(name);
    }
  }
  if (skippedCount > SKIPPED_REPORT_CAP) {
    result.skipped.push(`…and ${skippedCount - SKIPPED_REPORT_CAP} more`);
  }
  return result;
}
