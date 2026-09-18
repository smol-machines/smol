/** Real local-transport resize acceptance; requires a built addon and runtime.
 * Run explicitly with bun test/resize-local.ts on Linux x86_64 with KVM.
 * Uses no cloud credentials or network images; failures never become skips.
 */
import assert from "node:assert/strict";
import { Machine } from "../index";

async function main() {
  assert.equal(process.platform, "linux");
  assert.equal(process.arch, "x64");
  const machine = await Machine.create({
    resources: { cpus: 2, memoryMb: 512, storageGb: 1, overlayGb: 1 },
  }, { target: "local" });
  async function guest(command: string) {
    const result = await machine.exec(["sh", "-ec", command]);
    result.assertSuccess();
    return result.stdout.trim();
  }
  try {
    const boot = await guest("cat /proc/sys/kernel/random/boot_id");
    await guest("echo alive >/dev/shm/resize-marker; echo disk >/storage/resize-marker");
    await machine.resize({ cpus: 4 });
    assert.equal(await guest("taskset -c 3 echo cpu-active"), "cpu-active");
    await machine.resize({ memoryMb: 1024 });
    await guest("mount -o remount,size=800M /dev/shm; dd if=/dev/zero of=/dev/shm/grown bs=1M count=704; test $(stat -c %s /dev/shm/grown) = 738197504");
    await machine.resize({ storageGb: 2, overlayGb: 2 });
    await machine.resize({ storageGb: 2, overlayGb: 2 });
    await assert.rejects(() => machine.resize({ storageGb: 1 }));
    assert.equal(await guest("cat /proc/sys/kernel/random/boot_id"), boot);
    await guest("test $(cat /dev/shm/resize-marker) = alive; test $(cat /storage/resize-marker) = disk; test $(df -k /storage | tail -1 | awk '{print $2}') -gt 1900000; test $(df -k / | tail -1 | awk '{print $2}') -gt 1900000");
    console.log("local SDK CPU/RAM/disk growth and boot continuity passed");
  } finally {
    await machine.delete();
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
