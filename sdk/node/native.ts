/** Typed bridge to the generated NAPI addon.
 *
 *  At runtime this loads `./binding.js` (produced by `napi build` and copied
 *  into `dist/` by the `bundle:native` step). The native interface is declared
 *  here so the TypeScript layer type-checks **without** requiring the native
 *  build to have run — the generated `binding.d.ts` is not part of `tsc`'s graph.
 *
 *  Field names use napi-rs's default snake_case→camelCase conversion.
 */

export interface NativeEnvVar {
  key: string;
  value: string;
}

export interface NativeExecOptions {
  env?: NativeEnvVar[] | undefined;
  workdir?: string | undefined;
  timeoutSecs?: number | undefined;
}

export interface NativeHostMount {
  source: string;
  target: string;
  readOnly?: boolean | undefined;
  staged?: boolean | undefined;
}

export interface NativePortMapping {
  host: number;
  guest: number;
}

export interface NativeResources {
  cpus?: number | undefined;
  memoryMib?: number | undefined;
  network?: boolean | undefined;
  storageGib?: number | undefined;
  overlayGib?: number | undefined;
  gpu?: boolean | undefined;
  gpuVramMib?: number | undefined;
  cuda?: boolean | undefined;
}

export interface NativeMachineConfig {
  name: string;
  image?: string | undefined;
  env?: NativeEnvVar[] | undefined;
  workdir?: string | undefined;
  mounts?: NativeHostMount[] | undefined;
  ports?: NativePortMapping[] | undefined;
  resources?: NativeResources | undefined;
  persistent?: boolean | undefined;
  forkable?: boolean | undefined;
}

export interface NativeExecResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export interface NativeImageInfo {
  reference: string;
  digest: string;
  size: number;
  architecture: string;
  os: string;
}

export interface NativeCheckpointResult {
  sizeBytes: number;
  reusedBytes: number;
  sourcePauseMs: number;
  elapsedMs: number;
}

export interface NativeExecStreamEvent {
  kind: string;
  data?: string;
  exitCode?: number;
  message?: string;
}

/** A live exec stream: `next()` resolves the next event, or `null` at end. */
export interface NativeExecStream {
  next(): Promise<NativeExecStreamEvent | null>;
}

export interface NapiMachine {
  readonly name: string;
  readonly pid?: number;
  readonly isRunning: boolean;
  state(): string;
  hostPort(guestPort: number): number | null;
  guestPorts(): number[];
  start(interceptorAddress?: string, interceptorToken?: string): Promise<void>;
  startForkable(): Promise<void>;
  checkpoint(output: string, store?: string): Promise<NativeCheckpointResult>;
  fork(
    name: string,
    ports?: NativePortMapping[],
    checkpointable?: boolean,
  ): Promise<NapiMachine>;
  forkBatch(
    names: string[],
    ports?: NativePortMapping[],
    parallel?: number,
  ): Promise<NapiMachine[]>;
  exec(
    command: string[],
    options?: NativeExecOptions,
  ): Promise<NativeExecResult>;
  run(
    image: string,
    command: string[],
    options?: NativeExecOptions,
  ): Promise<NativeExecResult>;
  pullImage(image: string): Promise<NativeImageInfo>;
  listImages(): Promise<NativeImageInfo[]>;
  writeFile(
    path: string,
    data: Buffer,
    options?: { mode?: number },
  ): Promise<void>;
  readFile(path: string): Promise<Buffer>;
  execStream(command: string[], options?: NativeExecOptions): NativeExecStream;
  sync(): Promise<void>;
  stop(): Promise<void>;
  pause(): Promise<void>;
  resume(): Promise<void>;
  delete(): Promise<void>;
}

export interface NapiMachineCtor {
  new (config: NativeMachineConfig): NapiMachine;
  connect(name: string, interceptorAddress?: string, interceptorToken?: string): NapiMachine;
  restoreCheckpoint(name: string, artifact: string): NapiMachine;
  exportCheckpoint(source: string, output: string): number;
  pruneCheckpointStore(store: string): number;
}

import { PLATFORM_PACKAGES, wireBundledAssets, type RuntimeAssets } from "./assets";

/** Explain a failed addon load in terms of the per-platform package that
 *  carries it. The usual causes are an unsupported platform, an install with
 *  `--omit=optional`, or a lockfile written on another OS/arch that recorded no
 *  entry for this platform's package (a long-standing npm behavior with
 *  platform-gated optional dependencies). */
function missingPlatformPackageError(cause: unknown): Error {
  const platformArch = `${process.platform}-${process.arch}`;
  const pkg = PLATFORM_PACKAGES[platformArch];
  const detail = cause instanceof Error ? cause.message : String(cause);
  const hint = pkg
    ? `The local engine for ${platformArch} ships in the optional package "${pkg}", ` +
      `which is not installed. Reinstall without --omit=optional, or add it ` +
      `explicitly (npm install ${pkg}); if your lockfile was generated on another ` +
      `platform, regenerate it on this one.`
    : `No prebuilt local engine exists for ${platformArch}. The cloud transport ` +
      `(ConnectOptions({ target: "cloud" })) works on any platform.`;
  return new Error(`smolmachines: cannot load the native engine. ${hint}\n(${detail})`);
}

interface NativeBinding {
  NapiMachine: NapiMachineCtor;
  configureRuntimeAssets(assets: RuntimeAssets): void;
}

let cachedCtor: NapiMachineCtor | undefined;

/** Lazily load the native NAPI addon, caching it after the first call.
 *
 *  Deferred (rather than required at module load) so that importing the SDK for
 *  CLOUD-ONLY use never needs the platform-native binary to be present — the
 *  addon is loaded on first LOCAL machine create/connect. A pure cloud consumer
 *  on a platform with no prebuilt addon can therefore `import`/`require` the SDK
 *  and use `ConnectOptions({ target: "cloud" })` with no native build at all. */
export function getNapiMachine(): NapiMachineCtor {
  if (!cachedCtor) {
    // Wire the bundled boot helper + libs into the environment BEFORE the addon
    // loads, so the engine (which reads SMOLVM_BOOT_BINARY / SMOLVM_LIB_DIR at
    // spawn time) uses them.
    const assets = wireBundledAssets();
    let binding: NativeBinding;
    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      binding = require("./binding.js") as NativeBinding;
    } catch (error) {
      throw missingPlatformPackageError(error);
    }
    binding.configureRuntimeAssets(assets);
    cachedCtor = binding.NapiMachine;
  }
  return cachedCtor;
}
