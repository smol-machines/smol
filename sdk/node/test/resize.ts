import assert from "node:assert/strict";
import { validateResizeOptions } from "../transport";
import { Machine } from "../machine";
import { InvalidConfigError } from "../errors";
import type { ResizeOptions } from "../types";

async function main() {
  for (const input of [null, [], {}, { cpus: 0 }, { cpus: 1.5 }, { cpus: true },
    { cpus: NaN }, { cpus: Infinity }, { cpus: 256 }, { cpus: 4294967297 },
    { memoryMb: 2 ** 32 }, { storageGb: 2 ** 34 }, { unknown: 1 },
    { cpus: 4, memoryMb: 2048 }, { cpus: 4, constructor: 1 }, { overlayGb: -1 }]) {
    assert.throws(() => validateResizeOptions(input as ResizeOptions), InvalidConfigError);
  }
  validateResizeOptions({ storageGb: 4, overlayGb: 8 });
  validateResizeOptions({ memoryMb: 1280 });
  const calls: ResizeOptions[] = [];
  const local = Object.create(Machine.prototype);
  local.transport = { resize: async (options: ResizeOptions) => {
    validateResizeOptions(options);
    calls.push(options);
  } };
  await local.resize({ cpus: 4 });
  assert.deepEqual(calls, [{ cpus: 4 }]);
  await assert.rejects(() => local.resize({ cpus: 0 }), InvalidConfigError);
  assert.equal(calls.length, 1);
  local.transport.resize = async () => { throw new Error("unfinished resize"); };
  await assert.rejects(() => local.resize({ memoryMb: 1280 }), /unfinished resize/);
  console.log("resize client contract passed (no real VM in this test)");
}
main().catch(error => { console.error(error); process.exitCode = 1; });
