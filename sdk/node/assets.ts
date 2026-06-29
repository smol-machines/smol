/** Auto-wiring for bundled native assets.
 *
 *  Points the engine at the package's bundled, signed boot helper and hypervisor
 *  libraries so the SDK works on a plain `node` with no manual env setup:
 *    - SMOLVM_BOOT_BINARY → bundled `smol-vmm` helper (handles `_boot-vm`; on
 *      macOS codesigned with `com.apple.security.hypervisor`, so the user's
 *      `node` needs no entitlement).
 *    - SMOLVM_LIB_DIR     → the dir holding libkrun/libkrunfw.
 *    - SMOLVM_AGENT_ROOTFS_TAR → bundled guest rootfs tarball (the engine
 *      extracts it on first use), so a plain `npm i` is fully self-contained.
 *
 *  A user-provided value always wins. Exposed as a function (and self-invoked)
 *  so it runs reliably regardless of import elision/ordering — `native.ts` calls
 *  it before loading the addon.
 */

import { existsSync } from 'node:fs';
import { join } from 'node:path';

let wired = false;

export function wireBundledAssets(): void {
  if (wired) return;
  wired = true;

  const platformArch = `${process.platform}-${process.arch}`;
  const helperName = process.platform === 'win32' ? 'smol-vmm.exe' : 'smol-vmm';

  // `__dirname` is the package root from source (tsx) and `dist/` when built —
  // check both layouts.
  const candidates = [
    join(__dirname, 'native', platformArch),
    join(__dirname, '..', 'native', platformArch),
  ];

  for (const nativeDir of candidates) {
    if (!existsSync(nativeDir)) continue;
    const helper = join(nativeDir, helperName);
    if (!process.env.SMOLVM_BOOT_BINARY && existsSync(helper)) {
      process.env.SMOLVM_BOOT_BINARY = helper;
    }
    if (!process.env.SMOLVM_LIB_DIR) {
      process.env.SMOLVM_LIB_DIR = nativeDir;
    }
    const rootfsTar = join(nativeDir, 'agent-rootfs.tar');
    if (!process.env.SMOLVM_AGENT_ROOTFS && !process.env.SMOLVM_AGENT_ROOTFS_TAR && existsSync(rootfsTar)) {
      process.env.SMOLVM_AGENT_ROOTFS_TAR = rootfsTar;
    }
    return;
  }
}

wireBundledAssets();
