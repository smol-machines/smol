/**
 * smolmachines as an OpenAI Agents SDK sandbox client.
 *
 * ```ts
 * import { run } from '@openai/agents';
 * import { Manifest, SandboxAgent, shell } from '@openai/agents/sandbox';
 * import { SmolmachinesSandboxClient } from 'smolmachines/openai-agents';
 *
 * const client = new SmolmachinesSandboxClient({ image: 'python:3.12-slim', allowHosts: ['pypi.org'] });
 * const agent = new SandboxAgent({ name: 'Coder', model, capabilities: [shell()] });
 * const result = await run(agent, 'Run the tests.', { sandbox: { client } });
 * ```
 *
 * Each sandbox session is a local smolmachines microVM with its own Linux
 * kernel. The manifest's `file`, `dir`, `local_file` and `local_dir` entries
 * are written into the machine's workspace; `git_repo` entries, mounts, PTY
 * sessions and core snapshots are refused rather than ignored.
 *
 * Needs `@openai/agents-core` 0.18 (a peer of `@openai/agents`).
 */

import { randomUUID } from 'node:crypto';
import { lstat, readdir, readFile, realpath } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { applyDiff, type Editor } from '@openai/agents-core';
import {
  Manifest,
  normalizeSandboxClientCreateArgs,
  SandboxExecTimeoutError,
  SandboxProviderError,
  SandboxUnsupportedFeatureError,
  SandboxWorkspaceReadNotFoundError,
  WorkspacePathPolicy,
  type Entry,
  type ExecCommandArgs,
  type ListDirectoryArgs,
  type MaterializeEntryArgs,
  type ReadFileArgs,
  type SandboxClient,
  type SandboxClientCreateArgs,
  type SandboxClientOptions,
  type SandboxDirectoryEntry,
  type SandboxSession,
  type SandboxSessionState,
  type ViewImageArgs,
} from '@openai/agents-core/sandbox';
import {
  deserializeManifest,
  deserializePersistedEnvironmentForRuntime,
  elapsedSeconds,
  formatExecResponse,
  imageOutputFromBytes,
  isHostPathWithinRoot,
  materializeEnvironment,
  mergeManifestDelta,
  mergeManifestEntryDelta,
  posixDirname,
  sandboxPathGrantHostPath,
  serializeManifestRecord,
  serializeRuntimeEnvironmentForPersistence,
  truncateOutput,
  withExclusiveSandboxManifestMutation,
} from '@openai/agents-core/sandbox/internal';
import { Machine } from './machine.js';
import type { ConnectOptions } from './types.js';

/** Options for {@link SmolmachinesSandboxClient}. Per-run `sandbox.options` override them. */
export interface SmolmachinesSandboxClientOptions extends SandboxClientOptions {
  /** OCI image each session's machine boots. @default 'node:22-slim' */
  image?: string;
  /** vCPUs per machine. */
  cpus?: number;
  /** Memory per machine in MiB. */
  memoryMb?: number;
  /** Outbound network access. The image is pulled inside the machine, so a registry image needs it. @default true */
  network?: boolean;
  /**
   * Allow egress only to these hosts: exact names or `*.domain` wildcards,
   * which don't match the bare domain. Enforced outside the machine.
   */
  allowHosts?: string[];
  /** Allow egress only to these CIDR ranges. */
  allowCidrs?: string[];
  /** Environment variables for every command, under the manifest's own. */
  env?: Record<string, string>;
  /** Longest a command may run, in milliseconds. @default 600000 */
  commandTimeoutMs?: number;
  /**
   * Directory that `local_file` and `local_dir` sources must stay inside,
   * unless a manifest path grant covers them. @default process.cwd()
   */
  localSourceBaseDir?: string;
  /**
   * Keep a session's machine when the runner preserves the session (for
   * example on an interrupted run), so it can be resumed from serialized
   * state, even from another process. Otherwise the machine is deleted.
   * @default false
   */
  preserveOnExit?: boolean;
}

/** Serialized provider state: enough to find the machine again. */
interface SmolmachinesProviderState {
  machine: string;
}

/** Session state: the manifest and environment, plus the machine's name. */
export interface SmolmachinesSandboxSessionState extends SandboxSessionState {
  machine: string;
}

const BACKEND_ID = 'smolmachines';
const DEFAULT_IMAGE = 'node:22-slim';
const DEFAULT_COMMAND_TIMEOUT_MS = 600_000;
const DEFAULT_SHELL = '/bin/sh';
const LOCAL: ConnectOptions = { target: 'local', handleSignals: false };
const OWNER_LABEL = 'openai-agents-sandbox';

/** The OpenAI Agents SDK sandbox client for local smolmachines microVMs. */
export class SmolmachinesSandboxClient implements SandboxClient<SmolmachinesSandboxClientOptions, SmolmachinesSandboxSessionState> {
  readonly backendId = BACKEND_ID;
  private readonly defaults: SmolmachinesSandboxClientOptions;

  constructor(options: SmolmachinesSandboxClientOptions = {}) {
    this.defaults = { ...options };
  }

  async create(
    args?: SandboxClientCreateArgs<SmolmachinesSandboxClientOptions> | Manifest,
    manifestOptions?: SmolmachinesSandboxClientOptions,
  ): Promise<SmolmachinesSandboxSession> {
    const normalized = normalizeSandboxClientCreateArgs(args, manifestOptions);
    const options: SmolmachinesSandboxClientOptions = { ...this.defaults, ...normalized.options };
    if (normalized.snapshot && normalized.snapshot.type !== 'noop') {
      throw new SandboxUnsupportedFeatureError(
        `smolmachines sandboxes don't support ${normalized.snapshot.type} snapshots; checkpoint the machine instead`,
        { provider: BACKEND_ID },
      );
    }
    assertManifestSupported(normalized.manifest);
    return await this.createSession(normalized.manifest, options);
  }

  private async createSession(manifest: Manifest, options: SmolmachinesSandboxClientOptions): Promise<SmolmachinesSandboxSession> {
    const name = `openai-sbx-${randomUUID().slice(0, 12)}`;
    const restricted = (options.allowHosts?.length ?? 0) > 0 || (options.allowCidrs?.length ?? 0) > 0;
    const machine = await Machine.create(
      {
        name,
        image: options.image ?? DEFAULT_IMAGE,
        labels: { [OWNER_LABEL]: 'true' },
        ...(options.preserveOnExit ? { detach: true } : {}),
        resources: {
          ...(options.cpus !== undefined && { cpus: options.cpus }),
          ...(options.memoryMb !== undefined && { memoryMb: options.memoryMb }),
          network: (options.network ?? true) || restricted,
          ...(options.allowHosts?.length && { allowHosts: options.allowHosts }),
          ...(options.allowCidrs?.length && { allowCidrs: options.allowCidrs }),
        },
      },
      LOCAL,
    );
    try {
      const environment = await materializeEnvironment(manifest, options.env ?? {});
      const session = new SmolmachinesSandboxSession(
        machine,
        { manifest: new Manifest({ root: manifest.root }), environment, machine: name, workspaceReady: false },
        options,
      );
      await session.prepareWorkspace();
      await session.applyManifest(manifest);
      session.state.workspaceReady = true;
      return session;
    } catch (error) {
      await machine.delete().catch(() => {});
      throw error;
    }
  }

  canPersistOwnedSessionState(): boolean {
    return this.defaults.preserveOnExit === true;
  }

  async serializeSessionState(state: SmolmachinesSandboxSessionState): Promise<Record<string, unknown>> {
    // The runtime lifts the manifest and SDK fields into its envelope and
    // hands the same flat record back to deserializeSessionState().
    return {
      manifest: serializeManifestRecord(state.manifest),
      environment: serializeRuntimeEnvironmentForPersistence(state.manifest, state.environment ?? {}),
      workspaceReady: state.workspaceReady === true,
      machine: state.machine,
    } satisfies Record<string, unknown> & SmolmachinesProviderState;
  }

  async deserializeSessionState(record: Record<string, unknown>): Promise<SmolmachinesSandboxSessionState> {
    const provider = record as Partial<SmolmachinesProviderState>;
    if (typeof provider.machine !== 'string' || !provider.machine.startsWith('openai-sbx-')) {
      throw new SandboxProviderError('Serialized smolmachines session state names no machine', { provider: BACKEND_ID });
    }
    const manifest = deserializeManifest(record.manifest as Record<string, unknown> | undefined);
    return {
      manifest,
      environment: deserializePersistedEnvironmentForRuntime(
        manifest,
        record.environment as Record<string, string> | undefined,
        this.defaults.env ?? {},
      ),
      machine: provider.machine,
      workspaceReady: record.workspaceReady === true,
    };
  }

  async resume(state: SmolmachinesSandboxSessionState): Promise<SmolmachinesSandboxSession> {
    let machine: Machine;
    try {
      // Starts the machine again if it was stopped when the session was preserved.
      machine = await Machine.connect(state.machine, LOCAL);
    } catch (error) {
      throw new SandboxProviderError(`smolmachines machine '${state.machine}' can no longer be resumed`, {
        provider: BACKEND_ID,
        cause: (error as Error).message,
      });
    }
    return new SmolmachinesSandboxSession(machine, state, this.defaults);
  }

  async delete(state: SmolmachinesSandboxSessionState): Promise<void> {
    const machine = await Machine.connect(state.machine, LOCAL).catch(() => null);
    await machine?.delete();
  }
}

/** One sandbox session, backed by one microVM. */
export class SmolmachinesSandboxSession implements SandboxSession<SmolmachinesSandboxSessionState> {
  readonly state: SmolmachinesSandboxSessionState;
  private readonly machine: Machine;
  private readonly options: SmolmachinesSandboxClientOptions;
  private closed = false;

  constructor(machine: Machine, state: SmolmachinesSandboxSessionState, options: SmolmachinesSandboxClientOptions) {
    this.machine = machine;
    this.state = state;
    this.options = options;
  }

  /** The machine backing this session. */
  get machineName(): string {
    return this.state.machine;
  }

  /** Create the workspace root. */
  async prepareWorkspace(): Promise<void> {
    await this.run(['mkdir', '-p', this.state.manifest.root], 'create the workspace');
  }

  supportsPty(): boolean {
    return false;
  }

  async execCommand(args: ExecCommandArgs): Promise<string> {
    this.assertOpen();
    if (args.tty) {
      throw new SandboxUnsupportedFeatureError('smolmachines sandboxes do not support tty=true', { provider: BACKEND_ID });
    }
    const workdir = this.resolvePath(args.workdir);
    const shell = args.shell ?? DEFAULT_SHELL;
    const timeoutMs = this.options.commandTimeoutMs ?? DEFAULT_COMMAND_TIMEOUT_MS;
    const start = Date.now();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(new Error('timeout')), timeoutMs);
    let result: { exitCode: number; stdout: string; stderr: string };
    try {
      result = await this.machine.exec([shell, args.login === false ? '-c' : '-lc', args.cmd], {
        workdir,
        env: this.state.environment ?? {},
        signal: controller.signal,
        ...(args.runAs !== undefined && { user: args.runAs }),
      });
    } catch (error) {
      if (controller.signal.aborted) {
        throw new SandboxExecTimeoutError(`Command timed out after ${timeoutMs}ms`, {
          provider: BACKEND_ID,
          command: args.cmd,
          timeoutMs,
        });
      }
      throw new SandboxProviderError(`smolmachines exec failed: ${(error as Error).message}`, { provider: BACKEND_ID });
    } finally {
      clearTimeout(timer);
    }
    const output = truncateOutput(
      [result.stdout, result.stderr].filter((part) => part.trim().length > 0).join('\n'),
      args.maxOutputTokens,
    );
    return formatExecResponse({
      output: output.text,
      wallTimeSeconds: elapsedSeconds(start),
      exitCode: result.exitCode,
      ...(output.originalTokenCount !== undefined && { originalTokenCount: output.originalTokenCount }),
    });
  }

  createEditor(runAs?: string): Editor {
    this.assertOpen();
    this.assertNoRunAs(runAs);
    const text = new TextDecoder();
    return {
      createFile: async (operation) => {
        const path = this.resolvePath(operation.path, true);
        if (await this.pathExists(path)) {
          throw new SandboxProviderError(`Cannot create file because it already exists: ${path}`, { provider: BACKEND_ID });
        }
        await this.writeText(path, applyDiff('', operation.diff, 'create'));
        return {};
      },
      updateFile: async (operation) => {
        const path = this.resolvePath(operation.path, true);
        const destination = operation.moveTo ? this.resolvePath(operation.moveTo, true) : path;
        const current = text.decode(await this.readBytes(path));
        await this.writeText(destination, applyDiff(current, operation.diff));
        if (destination !== path) await this.run(['rm', '-f', '--', path], `remove ${path}`);
        return {};
      },
      deleteFile: async (operation) => {
        const path = this.resolvePath(operation.path, true);
        await this.run(['rm', '-f', '--', path], `delete ${path}`);
        return {};
      },
    };
  }

  async viewImage(args: ViewImageArgs) {
    this.assertOpen();
    this.assertNoRunAs(args.runAs);
    return imageOutputFromBytes(args.path, await this.readBytes(this.resolvePath(args.path)));
  }

  async readFile(args: ReadFileArgs): Promise<Uint8Array> {
    this.assertOpen();
    this.assertNoRunAs(args.runAs);
    const bytes = await this.readBytes(this.resolvePath(args.path));
    return typeof args.maxBytes === 'number' && bytes.byteLength > args.maxBytes ? bytes.subarray(0, args.maxBytes) : bytes;
  }

  async listDir(args: ListDirectoryArgs): Promise<SandboxDirectoryEntry[]> {
    this.assertOpen();
    this.assertNoRunAs(args.runAs);
    const dir = this.resolvePath(args.path);
    // NUL-separated `type<TAB>name` records; symlinks report as 'other'.
    const script =
      'for f in "$1"/* "$1"/.[!.]* "$1"/..?*; do [ -e "$f" ] || [ -L "$f" ] || continue; ' +
      'if [ -L "$f" ]; then t=other; elif [ -d "$f" ]; then t=dir; elif [ -f "$f" ]; then t=file; else t=other; fi; ' +
      'printf "%s\\t%s\\0" "$t" "${f##*/}"; done';
    const listed = await this.machine.exec(['sh', '-c', `[ -d "$1" ] || exit 3; ${script}`, 'list', dir]);
    if (listed.exitCode === 3) {
      throw new SandboxWorkspaceReadNotFoundError(`Directory not found: ${args.path}`, { provider: BACKEND_ID, path: dir });
    }
    if (listed.exitCode !== 0) {
      throw new SandboxProviderError(`Could not list ${dir}: ${listed.stderr.trim()}`, { provider: BACKEND_ID });
    }
    return listed.stdout
      .split('\0')
      .filter((record) => record.length > 0)
      .map((record) => {
        const [type, name] = [record.slice(0, record.indexOf('\t')), record.slice(record.indexOf('\t') + 1)];
        return {
          name,
          path: `${dir === '/' ? '' : dir}/${name}`,
          type: type === 'dir' || type === 'file' ? type : 'other',
        };
      });
  }

  async pathExists(path: string, runAs?: string): Promise<boolean> {
    this.assertOpen();
    this.assertNoRunAs(runAs);
    return this.probe(`[ -e "$1" ] || [ -L "$1" ]`, this.resolvePath(path));
  }

  async directoryExists(path: string, runAs?: string): Promise<boolean> {
    this.assertOpen();
    this.assertNoRunAs(runAs);
    return this.probe(`[ -d "$1" ]`, this.resolvePath(path));
  }

  async materializeEntry(args: MaterializeEntryArgs): Promise<void> {
    this.assertOpen();
    this.assertNoRunAs(args.runAs);
    assertEntrySupported(args.path, args.entry);
    await withExclusiveSandboxManifestMutation(this.state, async () => {
      await this.writeEntry(args.path, args.entry);
      this.state.manifest = mergeManifestEntryDelta(this.state.manifest, args.path, args.entry);
    });
  }

  async applyManifest(manifest: Manifest, runAs?: string): Promise<void> {
    this.assertOpen();
    this.assertNoRunAs(runAs);
    assertManifestSupported(manifest);
    await withExclusiveSandboxManifestMutation(this.state, async () => {
      for (const [path, entry] of Object.entries(manifest.entries)) {
        await this.writeEntry(path, entry, manifest);
      }
      this.state.manifest = mergeManifestDelta(this.state.manifest, manifest);
    });
  }

  async running(): Promise<boolean> {
    if (this.closed) return false;
    return (await this.machine.state().catch(() => 'unknown')) === 'running';
  }

  /** End the session: delete the machine, or stop it when the session is preserved. */
  async delete(options: { reason?: string; preserveOwnedSessions?: boolean } = {}): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.options.preserveOnExit && options.preserveOwnedSessions) {
      await this.machine.stop();
    } else {
      await this.machine.delete();
    }
  }

  async close(): Promise<void> {
    await this.delete();
  }

  // ---------------------------------------------------------------------------

  private assertOpen(): void {
    if (this.closed) {
      throw new SandboxProviderError(`smolmachines session '${this.state.machine}' is closed`, { provider: BACKEND_ID });
    }
  }

  private assertNoRunAs(runAs: string | undefined): void {
    if (runAs !== undefined) {
      throw new SandboxUnsupportedFeatureError('smolmachines sandboxes support runAs for commands only', { provider: BACKEND_ID });
    }
  }

  /** Resolve a sandbox path under the workspace root or a manifest path grant. */
  private resolvePath(path?: string, forWrite = false): string {
    const policy = new WorkspacePathPolicy({
      root: this.state.manifest.root,
      extraPathGrants: this.state.manifest.extraPathGrants,
    });
    return policy.resolve(path, { forWrite }).path;
  }

  private async probe(test: string, path: string): Promise<boolean> {
    const result = await this.machine.exec(['sh', '-c', `if ${test}; then exit 0; else exit 3; fi`, 'probe', path]);
    if (result.exitCode === 0) return true;
    if (result.exitCode === 3) return false;
    throw new SandboxProviderError(`Could not check ${path}: ${result.stderr.trim()}`, { provider: BACKEND_ID });
  }

  private async readBytes(path: string): Promise<Uint8Array> {
    if (!(await this.probe(`[ -f "$1" ]`, path))) {
      throw new SandboxWorkspaceReadNotFoundError(`File not found: ${path}`, { provider: BACKEND_ID, path });
    }
    return new Uint8Array(await this.machine.readFile(path));
  }

  private async writeText(path: string, content: string): Promise<void> {
    await this.writeBytes(path, new TextEncoder().encode(content));
  }

  private async writeBytes(path: string, content: Uint8Array): Promise<void> {
    const parent = posixDirname(path);
    if (parent !== '/' && parent !== '.') await this.run(['mkdir', '-p', '--', parent], `create ${parent}`);
    await this.machine.writeFile(path, content);
  }

  private async run(argv: string[], what: string): Promise<void> {
    const result = await this.machine.exec(argv);
    if (result.exitCode !== 0) {
      throw new SandboxProviderError(`Could not ${what}: ${result.stderr.trim()}`, { provider: BACKEND_ID });
    }
  }

  /** Write one manifest entry at a workspace-relative path. */
  private async writeEntry(relativePath: string, entry: Entry, manifest: Manifest = this.state.manifest): Promise<void> {
    const target = this.resolvePath(relativePath, true);
    switch (entry.type) {
      case 'file':
        await this.writeBytes(target, typeof entry.content === 'string' ? new TextEncoder().encode(entry.content) : entry.content);
        return;
      case 'dir':
        await this.run(['mkdir', '-p', '--', target], `create ${target}`);
        for (const [name, child] of Object.entries(entry.children ?? {})) {
          await this.writeEntry(`${relativePath.replace(/\/+$/, '')}/${name}`, child, manifest);
        }
        return;
      case 'local_file': {
        const source = await this.localSource('local_file', entry.src, manifest);
        const stat = await lstat(source);
        if (!stat.isFile()) throw new SandboxProviderError(`local_file source must be a regular file: ${source}`, { provider: BACKEND_ID });
        await this.writeBytes(target, await readFile(source));
        return;
      }
      case 'local_dir': {
        const source = await this.localSource('local_dir', entry.src, manifest);
        await this.copyLocalDir(source, target);
        return;
      }
      default:
        assertEntrySupported(relativePath, entry);
    }
  }

  /**
   * Resolve a host source for a local entry. Like the SDK's local clients, it
   * must stay inside the local source base directory or a manifest path
   * grant; the check uses the real path, so a symlink can't lead outside.
   */
  private async localSource(type: 'local_file' | 'local_dir', src: string, manifest: Manifest): Promise<string> {
    const base = await realpath(resolve(this.options.localSourceBaseDir ?? process.cwd()));
    const requested = resolve(base, src);
    if ((await lstat(requested)).isSymbolicLink()) {
      throw new SandboxProviderError(`${type} entries do not support symbolic links: ${requested}`, { provider: BACKEND_ID });
    }
    const real = await realpath(requested);
    const grants = await Promise.all(
      manifest.extraPathGrants.map(async (grant) => {
        const host = sandboxPathGrantHostPath(grant);
        return host ? await realpath(resolve(host)).catch(() => null) : null;
      }),
    );
    if (isHostPathWithinRoot(base, real) || grants.some((grant) => grant !== null && isHostPathWithinRoot(grant, real))) {
      return real;
    }
    throw new SandboxProviderError(
      `${type} source must stay within the local source base directory or manifest.extraPathGrants: ${real} (base: ${base})`,
      { provider: BACKEND_ID },
    );
  }

  private async copyLocalDir(source: string, target: string): Promise<void> {
    if (!(await lstat(source)).isDirectory()) {
      throw new SandboxProviderError(`local_dir source must be a directory: ${source}`, { provider: BACKEND_ID });
    }
    await this.run(['mkdir', '-p', '--', target], `create ${target}`);
    for (const child of await readdir(source, { withFileTypes: true })) {
      const from = join(source, child.name);
      const to = `${target}/${child.name}`;
      if (child.isSymbolicLink()) {
        throw new SandboxProviderError(`local_dir entries do not support symbolic links: ${from}`, { provider: BACKEND_ID });
      }
      if (child.isDirectory()) await this.copyLocalDir(from, to);
      else if (child.isFile()) await this.writeBytes(to, await readFile(from));
    }
  }
}

function assertManifestSupported(manifest: Manifest): void {
  if (manifest.users.length > 0 || manifest.groups.length > 0) {
    throw new SandboxUnsupportedFeatureError('smolmachines sandboxes do not support manifest users or groups', { provider: BACKEND_ID });
  }
  for (const [path, entry] of Object.entries(manifest.entries)) assertEntrySupported(path, entry);
}

function assertEntrySupported(path: string, entry: Entry): void {
  if (entry.type === 'file' || entry.type === 'local_file' || entry.type === 'local_dir' || entry.type === 'dir') {
    if (entry.group !== undefined || entry.permissions !== undefined) {
      throw new SandboxUnsupportedFeatureError(
        `smolmachines sandboxes do not support entry group or permissions (${path})`,
        { provider: BACKEND_ID, path },
      );
    }
    if (entry.type === 'dir') {
      for (const [name, child] of Object.entries(entry.children ?? {})) assertEntrySupported(`${path}/${name}`, child);
    }
    return;
  }
  throw new SandboxUnsupportedFeatureError(`smolmachines sandboxes do not support ${entry.type} entries (${path})`, {
    provider: BACKEND_ID,
    path,
  });
}
