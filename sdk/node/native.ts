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
  mounts?: NativeHostMount[] | undefined;
  ports?: NativePortMapping[] | undefined;
  resources?: NativeResources | undefined;
  persistent?: boolean | undefined;
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
  start(): Promise<void>;
  startForkable(): Promise<void>;
  fork(name: string, ports?: NativePortMapping[]): Promise<NapiMachine>;
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
  stop(): Promise<void>;
  delete(): Promise<void>;
}

export interface NapiMachineCtor {
  new (config: NativeMachineConfig): NapiMachine;
  connect(name: string): NapiMachine;
}

import { wireBundledAssets } from "./assets";

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
    wireBundledAssets();
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const binding = require("./binding.js") as { NapiMachine: NapiMachineCtor };
    cachedCtor = binding.NapiMachine;
  }
  return cachedCtor;
}
