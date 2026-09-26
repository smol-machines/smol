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
import { dirname, join } from 'node:path';

/** The per-platform npm package that carries the native addon, boot helper,
 *  hypervisor libraries and guest rootfs for each supported
 *  `${process.platform}-${process.arch}`. npm installs only the one matching
 *  the host (they are `optionalDependencies` gated by `os`/`cpu`/`libc`), so a
 *  user downloads one platform's runtime instead of all of them. Names follow
 *  the napi-rs triples `binding.js` already falls back to. */
export const PLATFORM_PACKAGES: Readonly<Record<string, string>> = {
  'darwin-arm64': 'smolmachines-darwin-arm64',
  'linux-x64': 'smolmachines-linux-x64-gnu',
  'linux-arm64': 'smolmachines-linux-arm64-gnu',
};

/** `native/` inside the installed platform package, or undefined when it is not
 *  installed (unsupported platform, `--omit=optional`, or a lockfile written on
 *  another platform that dropped it). */
function platformPackageNativeDir(platformArch: string): string | undefined {
  const pkg = PLATFORM_PACKAGES[platformArch];
  if (!pkg) return undefined;
  try {
    return join(dirname(require.resolve(`${pkg}/package.json`)), 'native');
  } catch {
    return undefined;
  }
}

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

  // A source checkout / CI build keeps the assets next to the package
  // (`__dirname` is the package root from source (tsx) and `dist/` when built —
  // check both layouts); a published install gets them from the per-platform
  // package.
  const candidates = [
    join(__dirname, 'native', platformArch),
    join(__dirname, '..', 'native', platformArch),
    platformPackageNativeDir(platformArch),
  ].filter((dir): dir is string => dir !== undefined);

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

wireBundledAssets();
