/**
 * smol — embed isolated microVM sandboxes directly in your code.
 *
 * ```ts
 * import { Machine } from 'smolmachines';
 *
 * const m = await Machine.create({ resources: { cpus: 2, memoryMb: 1024 } });
 * try {
 *   const res = await m.run('python:3.12', ['python', '-c', 'print(2 ** 10)']);
 *   console.log(res.stdout); // 1024
 * } finally {
 *   await m.delete();
 * }
 * ```
 *
 * Runs against the local embedded engine (default, no server) or the smolfleet
 * cloud — same API, backend selected via ConnectOptions / SMOL_CLOUD_TOKEN.
 */

export { Machine } from './machine';
export { SmolError, NotSupportedError, InvalidConfigError, ExecutionError } from './errors';
export type {
  MachineConfig,
  MachineState,
  ResourceSpec,
  MountSpec,
  PortSpec,
  ExecOptions,
  ExecResult,
  ImageInfo,
  ExecEvent,
  ConnectOptions,
  WaitReadyOptions,
  PortEndpoint,
} from './types';
