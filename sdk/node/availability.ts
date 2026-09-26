/** Can this host run local machines? Answered without booting one.
 *
 *  A framework choosing a sandbox (e.g. "local microVM if possible, else
 *  something else") otherwise learns only by attempting `Machine.create()`,
 *  which costs a full VM boot attempt to fail. This runs the same checks that
 *  boot would fail on and reports the same `error.code`, in order:
 *    1. a prebuilt local engine exists for this OS/arch (and, on Linux, libc);
 *    2. the native engine loads (its runtime is installed);
 *    3. the engine's own host checks pass — `/dev/kvm` access on Linux and a
 *       locatable libkrun everywhere.
 *  Never throws; the cloud target has no host requirements. */

import { getNapiMachine } from './native';

/** Why local machines cannot run here. `KVM_UNAVAILABLE` and
 *  `HYPERVISOR_UNAVAILABLE` match the codes a failing `create()` throws. */
export type LocalUnavailableCode =
  | 'UNSUPPORTED_PLATFORM'
  | 'RUNTIME_NOT_INSTALLED'
  | 'KVM_UNAVAILABLE'
  | 'HYPERVISOR_UNAVAILABLE';

export type LocalAvailability =
  | { available: true }
  | {
      available: false;
      /** A {@link LocalUnavailableCode}, or another engine error code. */
      code: LocalUnavailableCode | (string & {});
      /** Human-readable cause and remedy. */
      reason: string;
    };

/** `${process.platform}-${process.arch}` values with a prebuilt local engine. */
const SUPPORTED_PLATFORMS = new Set(['darwin-arm64', 'linux-x64', 'linux-arm64']);

/** Oldest glibc the Linux prebuilt engine runs on. */
const MIN_GLIBC: readonly [number, number] = [2, 34];

function unavailable(code: LocalUnavailableCode | string, reason: string): LocalAvailability {
  return { available: false, code, reason };
}

/** The runtime glibc version, or undefined on a non-glibc libc (e.g. musl). */
function glibcVersion(): string | undefined {
  const report = process.report?.getReport() as
    | { header?: { glibcVersionRuntime?: string } }
    | undefined;
  return report?.header?.glibcVersionRuntime;
}

function glibcAtLeast(version: string, [major, minor]: readonly [number, number]): boolean {
  const [a = 0, b = 0] = version.split('.').map(Number);
  return a > major || (a === major && b >= minor);
}

export function localAvailability(): LocalAvailability {
  const platformArch = `${process.platform}-${process.arch}`;
  if (!SUPPORTED_PLATFORMS.has(platformArch)) {
    return unavailable(
      'UNSUPPORTED_PLATFORM',
      `No prebuilt local engine for ${platformArch} (supported: macOS Apple Silicon, ` +
        `Linux x64/arm64 with glibc). The cloud target works on any platform.`,
    );
  }
  if (process.platform === 'linux') {
    const glibc = glibcVersion();
    if (!glibc) {
      return unavailable(
        'UNSUPPORTED_PLATFORM',
        'The local engine needs glibc; this Linux uses another libc (e.g. musl/Alpine).',
      );
    }
    if (!glibcAtLeast(glibc, MIN_GLIBC)) {
      return unavailable(
        'UNSUPPORTED_PLATFORM',
        `The local engine needs glibc >= ${MIN_GLIBC.join('.')}; this host has ${glibc}.`,
      );
    }
  }

  let engine: ReturnType<typeof getNapiMachine>;
  try {
    engine = getNapiMachine();
  } catch (error) {
    return unavailable(
      'RUNTIME_NOT_INSTALLED',
      `The local engine could not be loaded: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  const host = engine.checkHost();
  return host.available
    ? { available: true }
    : unavailable(host.code ?? 'HYPERVISOR_UNAVAILABLE', host.reason ?? 'The host cannot run local machines.');
}
