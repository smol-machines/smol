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
 *  A user-provided value always wins. Called by `native.ts` right before the
 *  addon loads — i.e. on first LOCAL use, never at import — so importing the
 *  SDK (e.g. for cloud-only use) leaves `process.env` untouched.
 */

import { existsSync } from 'node:fs';
import { join } from 'node:path';

export interface RuntimeAssets {
  bootBinary?: string;
  libDir?: string;
  agentRootfs?: string;
  agentRootfsTar?: string;
}

export function wireBundledAssets(): RuntimeAssets {
  const assets: RuntimeAssets = {};
  if (process.env.SMOLVM_BOOT_BINARY) assets.bootBinary = process.env.SMOLVM_BOOT_BINARY;
  if (process.env.SMOLVM_LIB_DIR) assets.libDir = process.env.SMOLVM_LIB_DIR;
  if (process.env.SMOLVM_AGENT_ROOTFS) {
    assets.agentRootfs = process.env.SMOLVM_AGENT_ROOTFS;
  } else if (process.env.SMOLVM_AGENT_ROOTFS_TAR) {
    assets.agentRootfsTar = process.env.SMOLVM_AGENT_ROOTFS_TAR;
  }

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
    if (!assets.bootBinary && existsSync(helper)) {
      assets.bootBinary = helper;
      process.env.SMOLVM_BOOT_BINARY = helper;
    }
    if (!assets.libDir) {
      assets.libDir = nativeDir;
      process.env.SMOLVM_LIB_DIR = nativeDir;
    }
    const rootfsTar = join(nativeDir, 'agent-rootfs.tar');
    if (!assets.agentRootfs && !assets.agentRootfsTar && existsSync(rootfsTar)) {
      assets.agentRootfsTar = rootfsTar;
      process.env.SMOLVM_AGENT_ROOTFS_TAR = rootfsTar;
    }
    break;
  }
  wireDefaultHardening();
  return assets;
}

/** Confine the spawned VMM by default on Linux.
 *
 *  `smol-vmm _boot-vm` reads SMOLVM_SECCOMP / SMOLVM_LANDLOCK and treats unset
 *  as OFF, which `smolvm serve` compensates for by defaulting both to
 *  "enforce". An embedding app got neither, so the SDK — whose whole purpose is
 *  running untrusted code — was the least confined way to boot a VM. Default
 *  them on here so `npm i smolmachines` is hardened out of the box; an
 *  explicitly-set value always wins, so `SMOLVM_SECCOMP=off` remains the escape
 *  hatch for a workload the allowlist does not cover.
 *
 *  Linux only: seccomp filtering is x86_64-Linux and Landlock is Linux; on
 *  macOS the helper ignores both, so setting them would only be misleading.
 */
export function wireDefaultHardening(): void {
  if (process.platform !== 'linux') return;
  process.env.SMOLVM_SECCOMP ??= 'enforce';
  process.env.SMOLVM_LANDLOCK ??= 'enforce';
}
