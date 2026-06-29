/** The public `Machine` — an isolated microVM, local (embedded) or cloud.
 *
 *  The backend is chosen by `ConnectOptions`:
 *    - default / `{ target: 'local' }` → embedded engine, no server.
 *    - `{ target: 'cloud', apiKey }` (or `SMOL_CLOUD_TOKEN`) → smolfleet cloud.
 *
 *  Both go through the same `Transport`, so calling code is identical; features
 *  a given backend lacks throw `NotSupportedError`.
 */

import { ExecutionError } from "./errors";
import {
  makeTransport,
  connectTransport,
  type RawExec,
  type Transport,
} from "./transport";
import type {
  ConnectOptions,
  ExecEvent,
  ExecOptions,
  ExecResult,
  ImageInfo,
  MachineConfig,
  PortSpec,
} from "./types";

function makeExecResult(r: RawExec): ExecResult {
  const success = r.exitCode === 0;
  return {
    exitCode: r.exitCode,
    stdout: r.stdout,
    stderr: r.stderr,
    success,
    output: r.stdout + r.stderr,
    assertSuccess() {
      if (!success) throw new ExecutionError(r.exitCode, r.stdout, r.stderr);
    },
  };
}

export class Machine {
  private constructor(private readonly transport: Transport) {}

  /**
   * Create and start a machine.
   *
   * @param config  machine configuration (a name is generated if omitted; `image`
   *                is the base image — required for cloud, optional for local)
   * @param conn    backend selection (local embedded by default)
   */
  static async create(
    config: MachineConfig = {},
    conn: ConnectOptions = {},
  ): Promise<Machine> {
    return new Machine(await makeTransport(config, conn));
  }

  /**
   * Attach to an EXISTING machine without creating a new one — to drive a
   * machine made elsewhere (another process, the console, the REST API).
   *  - local (default): re-opens a persisted machine by NAME, starting it if
   *    stopped — pairs with `Machine.create({ name, … }, …)` + `persistent`.
   *  - cloud: looks up the machine by id; throws if it doesn't exist.
   *
   * @param id    local machine name, or cloud machine id (`mach-…`)
   * @param conn  backend selection (local by default; cloud via `{ target: 'cloud', apiKey }` or `SMOL_CLOUD_TOKEN`)
   */
  static async connect(
    id: string,
    conn: ConnectOptions = {},
  ): Promise<Machine> {
    return new Machine(await connectTransport(id, conn));
  }

  /** The machine's name / identifier. */
  get name(): string {
    return this.transport.name;
  }

  /** Current state (e.g. "running" | "stopped"). */
  state(): Promise<string> {
    return this.transport.state();
  }

  /** Public ingress URL for the machine's first published port (cloud).
   *  `null` until the machine is started with an allocated host port, for
   *  machines with no published port, or on the local target (no public
   *  ingress). Reach the deployed app over HTTPS at the returned URL. */
  url(): Promise<string | null> {
    return this.transport.url();
  }

  /** Execute a command directly in the machine. */
  async exec(command: string[], opts?: ExecOptions): Promise<ExecResult> {
    return makeExecResult(await this.transport.exec(command, opts));
  }

  /** Pull an image (if needed) and run a command in a container of it. (local) */
  async run(
    image: string,
    command: string[],
    opts?: ExecOptions,
  ): Promise<ExecResult> {
    return makeExecResult(await this.transport.run(image, command, opts));
  }

  /** Execute a command, yielding stdout/stderr/exit events. (local) */
  execStream(command: string[], opts?: ExecOptions): AsyncGenerator<ExecEvent> {
    return this.transport.execStream(command, opts);
  }

  /** Read a file from the machine. */
  readFile(path: string): Promise<Buffer> {
    return this.transport.readFile(path);
  }

  /** Write a file into the machine. */
  writeFile(
    path: string,
    data: string | Uint8Array,
    mode?: number,
  ): Promise<void> {
    const buf =
      typeof data === "string" ? Buffer.from(data) : Buffer.from(data);
    return this.transport.writeFile(path, buf, mode);
  }

  /** Pull an OCI image into the machine's storage. (local) */
  pullImage(image: string): Promise<ImageInfo> {
    return this.transport.pullImage(image);
  }

  /** List cached OCI images. (local) */
  listImages(): Promise<ImageInfo[]> {
    return this.transport.listImages();
  }

  /** Stop the machine. */
  stop(): Promise<void> {
    return this.transport.stop();
  }

  /** Stop the machine and delete its storage. */
  delete(): Promise<void> {
    return this.transport.delete();
  }

  /** Fork this running, forkable machine into a new clone via copy-on-write live
   *  RAM + disks (cloud target). The clone inherits the golden's warm in-memory
   *  state and runs on the same node; forks are fast (~tens of ms) and repeatable
   *  from one golden — the basis for RL rollout branching and instant episode
   *  reset. The golden must have been created with `MachineConfig({ forkable: true })`.
   *
   *  @param name  name for the new clone machine.
   *  @param ports optional pinned inbound port forwards (`{ host, guest }`); by
   *               default the node allocates fresh host ports so clones don't collide.
   *  @returns a `Machine` handle to the running clone. */
  async fork(name: string, ports?: PortSpec[]): Promise<Machine> {
    return new Machine(await this.transport.fork(name, ports));
  }
}
