/** Transport layer — the seam that lets one `Machine` API drive either the
 *  embedded local engine or the smolfleet cloud, chosen by `ConnectOptions`.
 *
 *  - LocalTransport: in-process engine via the native addon (no server).
 *  - CloudTransport: REST client to smolfleet `/v1` (Bearer `smk_…`).
 *
 *  Cloud-only/local-only capability gaps surface as `NotSupportedError`.
 */

import {
  getNapiMachine,
  type NapiMachine as NapiInstance,
  type NativeExecOptions,
  type NativeMachineConfig,
} from "./native";
import {
  InvalidConfigError,
  NotSupportedError,
  SmolError,
  wrapNativeError,
} from "./errors";
import type {
  ConnectOptions,
  ExecEvent,
  ExecOptions,
  ImageInfo,
  MachineConfig,
  PortSpec,
} from "./types";
// Cloud wire shapes are generated from smolfleet's OpenAPI document (npm run
// gen:openapi → generated/smolfleet.ts), itself derived from the shared
// `smolfleet-api` Rust types. Typing the cloud payloads against these makes the
// type checker flag any drift from the server contract.
import type { components } from "./generated/smolfleet";

type Schemas = components["schemas"];
type CreateMachineRequest = Schemas["CreateMachineRequest"];
type MachineInfo = Schemas["MachineInfo"];
type MachineCommandRequest = Schemas["MachineCommandRequest"];
type MachineExecResponse = Schemas["MachineExecResponse"];

/** Raw exec result (the ergonomic wrapper is added in machine.ts). */
export interface RawExec {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export interface Transport {
  readonly name: string;
  state(): Promise<string>;
  url(): Promise<string | null>;
  exec(command: string[], opts?: ExecOptions): Promise<RawExec>;
  run(image: string, command: string[], opts?: ExecOptions): Promise<RawExec>;
  execStream(command: string[], opts?: ExecOptions): AsyncGenerator<ExecEvent>;
  readFile(path: string): Promise<Buffer>;
  writeFile(path: string, data: Buffer, mode?: number): Promise<void>;
  pullImage(image: string): Promise<ImageInfo>;
  listImages(): Promise<ImageInfo[]>;
  stop(): Promise<void>;
  delete(): Promise<void>;
  fork(name: string, ports?: PortSpec[]): Promise<Transport>;
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

function generateName(): string {
  const rand = Math.random().toString(36).slice(2, 8);
  return `smol-${Date.now().toString(36)}-${rand}`;
}

function toNativeExecOptions(
  opts?: ExecOptions,
): NativeExecOptions | undefined {
  if (!opts) return undefined;
  return {
    workdir: opts.workdir,
    timeoutSecs: opts.timeout,
    env:
      opts.env &&
      Object.entries(opts.env).map(([key, value]) => ({ key, value })),
  };
}

export function toNativeConfig(
  name: string,
  config: MachineConfig,
): NativeMachineConfig {
  return {
    name,
    persistent: config.persistent,
    mounts: config.mounts?.map((m) => ({
      source: m.source,
      target: m.target,
      // Prefer the canonical camelCase `readOnly`; fall back to the deprecated
      // lowercase `readonly` for backwards compatibility. Undefined → engine
      // default (writable).
      readOnly: m.readOnly ?? m.readonly,
    })),
    ports: config.ports?.map((p) => ({ host: p.host, guest: p.guest })),
    resources: config.resources && {
      cpus: config.resources.cpus,
      memoryMib: config.resources.memoryMb,
      network: config.resources.network,
      storageGib: config.resources.storageGb,
      overlayGib: config.resources.overlayGb,
      gpu: config.resources.gpu,
      gpuVramMib: config.resources.gpuVramMib,
    },
  };
}

// ---------------------------------------------------------------------------
// Local (embedded engine)
// ---------------------------------------------------------------------------

// Live local machines, stopped if the process is interrupted (Ctrl-C / SIGTERM)
// without an explicit delete()/stop(). The engine's parent-death watchdog
// already prevents a hard leak — it reaps the VMM ~500ms after we die — so this
// is best-effort GRACEFUL teardown: immediate and clean on signals. We hook
// SIGINT/SIGTERM (not 'exit', which is synchronous-only and can't await the
// engine's async stop()); safe because the native calls run on worker threads,
// leaving the event loop free to service the signal even mid-exec. Local only —
// cloud machines are remote and intentionally outlive this process.
const liveLocal = new Set<LocalTransport>();
let cleanupInstalled = false;

async function stopAllLocal(): Promise<void> {
  await Promise.allSettled([...liveLocal].map((t) => t.stop()));
}

function installLocalCleanup(): void {
  if (cleanupInstalled) return;
  cleanupInstalled = true;
  const onSignal = (sig: NodeJS.Signals) => {
    void stopAllLocal().finally(() => {
      // Re-raise: our once-listener is already removed, so the app's own handler
      // (or Node's default termination) now applies, with correct exit code.
      process.kill(process.pid, sig);
    });
  };
  process.once("SIGINT", () => onSignal("SIGINT"));
  process.once("SIGTERM", () => onSignal("SIGTERM"));
}

class LocalTransport implements Transport {
  constructor(private readonly inner: NapiInstance) {
    liveLocal.add(this);
    installLocalCleanup();
  }

  get name(): string {
    return this.inner.name;
  }

  async state(): Promise<string> {
    return this.inner.state();
  }

  async url(): Promise<string | null> {
    // Local machines have no public ingress URL — that's a cloud feature.
    return null;
  }

  async exec(command: string[], opts?: ExecOptions): Promise<RawExec> {
    try {
      return await this.inner.exec(command, toNativeExecOptions(opts));
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async run(
    image: string,
    command: string[],
    opts?: ExecOptions,
  ): Promise<RawExec> {
    try {
      return await this.inner.run(image, command, toNativeExecOptions(opts));
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async *execStream(
    command: string[],
    opts?: ExecOptions,
  ): AsyncGenerator<ExecEvent> {
    let stream;
    try {
      stream = this.inner.execStream(command, toNativeExecOptions(opts));
    } catch (e) {
      throw wrapNativeError(e);
    }
    // Pull events live as they arrive; null marks end-of-stream (command exit).
    for (;;) {
      let e;
      try {
        e = await stream.next();
      } catch (err) {
        throw wrapNativeError(err);
      }
      if (e === null) return;
      if (e.kind === "stdout" || e.kind === "stderr")
        yield { kind: e.kind, data: e.data ?? "" };
      else if (e.kind === "exit")
        yield { kind: "exit", exitCode: e.exitCode ?? 0 };
      else yield { kind: "error", message: e.message ?? "unknown error" };
    }
  }

  async readFile(path: string): Promise<Buffer> {
    try {
      return await this.inner.readFile(path);
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async writeFile(path: string, data: Buffer, mode?: number): Promise<void> {
    try {
      await this.inner.writeFile(
        path,
        data,
        mode === undefined ? undefined : { mode },
      );
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async pullImage(image: string): Promise<ImageInfo> {
    try {
      return await this.inner.pullImage(image);
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async listImages(): Promise<ImageInfo[]> {
    try {
      return await this.inner.listImages();
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async stop(): Promise<void> {
    liveLocal.delete(this);
    try {
      await this.inner.stop();
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async delete(): Promise<void> {
    liveLocal.delete(this);
    try {
      await this.inner.delete();
    } catch (e) {
      throw wrapNativeError(e);
    }
  }

  async fork(name: string, ports?: PortSpec[]): Promise<Transport> {
    // Local live-RAM CoW clone via the embedded engine. The golden must have been
    // started forkable (MachineConfig({ forkable: true })).
    const nativePorts = (ports ?? []).map((p) => ({
      host: p.host,
      guest: p.guest,
    }));
    try {
      const cloneInner = await this.inner.fork(name, nativePorts);
      return new LocalTransport(cloneInner); // ctor registers for cleanup
    } catch (e) {
      throw wrapNativeError(e);
    }
  }
}

// ---------------------------------------------------------------------------
// Cloud (smolfleet /v1)
// ---------------------------------------------------------------------------

interface CloudConn {
  baseUrl: string;
  apiKey: string;
}

const DEFAULT_CLOUD_URL = "https://api.smolmachines.com";

/** Default per-request timeout for cloud calls (ms). Override via opts.timeoutMs. */
const CLOUD_TIMEOUT_MS = 30_000;

/** Extra slack added on top of a command's own timeout when sizing the exec
 *  request timeout (ms), covering network round-trip and server-side overhead so
 *  the client never aborts before the server has had a chance to finish. */
const CLOUD_EXEC_TIMEOUT_HEADROOM_MS = 30_000;

/** Percent-encode each path segment but keep the `/` separators — the smolfleet
 *  files route is a wildcard (`/files/<path>`), so slashes are meaningful while
 *  spaces/?/#/% in a filename must be escaped. */
export function encodePath(p: string): string {
  return p.split("/").map(encodeURIComponent).join("/");
}

async function cloudFetch<T = unknown>(
  conn: CloudConn,
  method: string,
  path: string,
  opts: {
    json?: unknown;
    body?: Buffer;
    accept?: "json" | "bytes";
    timeoutMs?: number;
  } = {},
): Promise<T> {
  const headers: Record<string, string> = {
    authorization: `Bearer ${conn.apiKey}`,
  };
  let body: BodyInit | undefined;
  if (opts.json !== undefined) {
    headers["content-type"] = "application/json";
    body = JSON.stringify(opts.json);
  } else if (opts.body !== undefined) {
    headers["content-type"] = "application/octet-stream";
    body = opts.body;
  }

  // Bound every request so a hung network call can't block the caller forever.
  const controller = new AbortController();
  const timer = setTimeout(
    () => controller.abort(),
    opts.timeoutMs ?? CLOUD_TIMEOUT_MS,
  );
  let res: Response;
  try {
    res = await fetch(`${conn.baseUrl}${path}`, {
      method,
      headers,
      body: body ?? null,
      signal: controller.signal,
    });
  } catch (e) {
    if ((e as Error).name === "AbortError") {
      throw new SmolError(
        "TIMEOUT",
        `cloud ${method} ${path} timed out after ${opts.timeoutMs ?? CLOUD_TIMEOUT_MS}ms`,
      );
    }
    throw new SmolError(
      "CONNECTION",
      `cloud request failed: ${(e as Error).message}`,
    );
  } finally {
    clearTimeout(timer);
  }
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new SmolError(
      res.status === 404
        ? "NOT_FOUND"
        : res.status === 401
          ? "UNAUTHORIZED"
          : "SMOLVM_ERROR",
      `cloud ${method} ${path} → ${res.status}${text ? `: ${text}` : ""}`,
    );
  }
  if (opts.accept === "bytes") return Buffer.from(await res.arrayBuffer()) as T;
  if (res.status === 204) return undefined as T;
  const ct = res.headers.get("content-type") ?? "";
  return (ct.includes("application/json") ? await res.json() : undefined) as T;
}

/** Poll a cloud machine until it is started/running; throw on error state or
 *  timeout. Mirrors the Python SDK's `_wait_for_ready`. Auth/not-found errors are
 *  fatal (re-thrown immediately); other errors are treated as transient booting. */
async function waitForReady(
  conn: CloudConn,
  id: string,
  timeoutMs = 120_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    let state: string | undefined;
    try {
      const m = await cloudFetch<MachineInfo>(
        conn,
        "GET",
        `/v1/machines/${id}`,
      );
      state = m?.state ?? undefined;
    } catch (e) {
      if (
        e instanceof SmolError &&
        (e.code === "UNAUTHORIZED" || e.code === "NOT_FOUND")
      )
        throw e;
      // transient while booting — keep polling
    }
    if (state === "started" || state === "running") return;
    if (state === "error") {
      throw new SmolError(
        "SMOLVM_ERROR",
        `machine ${id} entered error state while starting`,
      );
    }
    if (Date.now() >= deadline) {
      throw new SmolError(
        "TIMEOUT",
        `machine ${id} not ready after ${timeoutMs}ms (state=${state ?? "unknown"})`,
      );
    }
    await new Promise((r) => setTimeout(r, 1000));
  }
}

class CloudTransport implements Transport {
  constructor(
    private readonly conn: CloudConn,
    public readonly name: string,
    private readonly id: string,
  ) {}

  async state(): Promise<string> {
    const m = await cloudFetch<MachineInfo>(
      this.conn,
      "GET",
      `/v1/machines/${this.id}`,
    );
    return m?.state ?? "unknown";
  }

  async url(): Promise<string | null> {
    // Public ingress URL for the first published port; null until the machine
    // is started with an allocated host port (and the control plane advertises
    // a public base URL).
    const m = await cloudFetch<MachineInfo & { url?: string | null }>(
      this.conn,
      "GET",
      `/v1/machines/${this.id}`,
    );
    return m?.url ?? null;
  }

  async exec(command: string[], opts?: ExecOptions): Promise<RawExec> {
    // smolfleet MachineCommandRequest: command (CommandSpec — argv array),
    // cwd, env, timeoutSeconds. (exactOptionalPropertyTypes: coerce undefined → null.)
    const json: MachineCommandRequest = {
      command,
      env: opts?.env ?? {},
      cwd: opts?.workdir ?? null,
      timeoutSeconds: opts?.timeout ?? null,
    };
    // The command may legitimately run far longer than the default cloud
    // timeout, so size the request abort timeout off the request's own timeout
    // (plus headroom) — never below the default. The server-sent timeoutSeconds
    // above is left untouched.
    const timeoutMs =
      opts?.timeout !== undefined
        ? Math.max(
            CLOUD_TIMEOUT_MS,
            opts.timeout * 1000 + CLOUD_EXEC_TIMEOUT_HEADROOM_MS,
          )
        : CLOUD_TIMEOUT_MS;
    const r = await cloudFetch<MachineExecResponse>(
      this.conn,
      "POST",
      `/v1/machines/${this.id}/exec`,
      {
        json,
        timeoutMs,
      },
    );
    return {
      exitCode: r.exitCode ?? 0,
      stdout: r.stdout ?? "",
      stderr: r.stderr ?? "",
    };
  }

  async run(
    _image: string,
    _command: string[],
    _opts?: ExecOptions,
  ): Promise<RawExec> {
    throw new NotSupportedError(
      "run(image, …) is not available on the cloud target; create a machine from an image " +
        "via Machine.create({ image }, { target: 'cloud' }) and use exec() instead.",
    );
  }

  async *execStream(
    command: string[],
    opts?: ExecOptions,
  ): AsyncGenerator<ExecEvent> {
    const json: MachineCommandRequest = {
      command,
      env: opts?.env ?? {},
      cwd: opts?.workdir ?? null,
      timeoutSeconds: opts?.timeout ?? null,
    };
    let res: Response;
    try {
      res = await fetch(
        `${this.conn.baseUrl}/v1/machines/${this.id}/exec/stream`,
        {
          method: "POST",
          headers: {
            authorization: `Bearer ${this.conn.apiKey}`,
            "content-type": "application/json",
            accept: "text/event-stream",
          },
          body: JSON.stringify(json),
        },
      );
    } catch (e) {
      throw new SmolError(
        "CONNECTION",
        `cloud exec/stream failed: ${(e as Error).message}`,
      );
    }
    if (!res.ok) {
      const text = await res.text().catch(() => "");
      throw new SmolError(
        res.status === 404
          ? "NOT_FOUND"
          : res.status === 401
            ? "UNAUTHORIZED"
            : "SMOLVM_ERROR",
        `cloud POST /v1/machines/${this.id}/exec/stream → ${res.status}${text ? `: ${text}` : ""}`,
      );
    }
    if (!res.body)
      throw new SmolError(
        "SMOLVM_ERROR",
        "exec/stream returned no response body",
      );

    // Parse the server's SSE stream: each event is `event: <kind>` + one or more
    // `data:` lines, terminated by a blank line. Multiple `data:` lines join with
    // `\n` (SSE spec); the `exit` event's data is JSON `{ exitCode }`.
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let event = "";
    let dataLines: string[] = [];
    const flush = (): ExecEvent | undefined => {
      const kind = event;
      const data = dataLines.join("\n");
      event = "";
      dataLines = [];
      if (kind === "stdout") return { kind: "stdout", data };
      if (kind === "stderr") return { kind: "stderr", data };
      if (kind === "error") return { kind: "error", message: data };
      if (kind === "exit") {
        let exitCode = 0;
        try {
          exitCode = (JSON.parse(data) as { exitCode?: number }).exitCode ?? 0;
        } catch {
          /* leave 0 */
        }
        return { kind: "exit", exitCode };
      }
      return undefined;
    };
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let nl: number;
        while ((nl = buf.indexOf("\n")) >= 0) {
          const raw = buf.slice(0, nl);
          buf = buf.slice(nl + 1);
          const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
          if (line === "") {
            const ev = flush();
            if (ev) yield ev;
          } else if (line.startsWith("event:")) {
            event = line.slice(6).trim();
          } else if (line.startsWith("data:")) {
            dataLines.push(line.slice(5).replace(/^ /, ""));
          }
        }
      }
      const ev = flush();
      if (ev) yield ev;
    } finally {
      reader.releaseLock();
    }
  }

  async readFile(path: string): Promise<Buffer> {
    return cloudFetch<Buffer>(
      this.conn,
      "GET",
      `/v1/machines/${this.id}/files/${encodePath(path)}`,
      {
        accept: "bytes",
      },
    );
  }

  async writeFile(path: string, data: Buffer, mode?: number): Promise<void> {
    await cloudFetch(
      this.conn,
      "PUT",
      `/v1/machines/${this.id}/files/${encodePath(path)}`,
      {
        body: data,
      },
    );
    // The cloud /files PUT carries no file mode, so apply it with chmod when
    // requested — e.g. writing an executable script the caller then runs.
    if (mode !== undefined) {
      await this.exec(["chmod", mode.toString(8), path]);
    }
  }

  async pullImage(_image: string): Promise<ImageInfo> {
    throw new NotSupportedError(
      "pullImage is not available on the cloud target.",
    );
  }

  async listImages(): Promise<ImageInfo[]> {
    throw new NotSupportedError(
      "listImages is not available on the cloud target.",
    );
  }

  async stop(): Promise<void> {
    await cloudFetch(this.conn, "POST", `/v1/machines/${this.id}/stop`);
  }

  async delete(): Promise<void> {
    await cloudFetch(this.conn, "DELETE", `/v1/machines/${this.id}`);
  }

  async fork(name: string, ports?: PortSpec[]): Promise<Transport> {
    // Live-RAM CoW clone on the golden's node. The control plane returns the
    // running clone; wait for its agent so the returned handle is usable.
    const portBody = (ports ?? []).map((p) => ({
      port: p.guest,
      hostPort: p.host,
    }));
    const clone = await cloudFetch<MachineInfo>(
      this.conn,
      "POST",
      `/v1/machines/${this.id}/fork`,
      {
        json: { name, ports: portBody },
      },
    );
    const cloneId = clone.id;
    const cloneName = clone.name ?? name;
    await waitForReady(this.conn, cloneId);
    return new CloudTransport(this.conn, cloneName, cloneId);
  }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/** Build and start the right transport for the requested target. */
export async function makeTransport(
  config: MachineConfig,
  conn: ConnectOptions,
): Promise<Transport> {
  const apiKey = conn.apiKey ?? process.env.SMOL_CLOUD_TOKEN;
  const useCloud =
    conn.target === "cloud" || (conn.target !== "local" && Boolean(apiKey));

  if (useCloud) {
    if (!apiKey) {
      throw new InvalidConfigError(
        "cloud target requires an API key — pass { apiKey } or set SMOL_CLOUD_TOKEN (run 'smol login').",
      );
    }
    if (!config.image) {
      throw new InvalidConfigError(
        'cloud target requires an image — pass { image } to Machine.create({ image }, { target: "cloud" }).',
      );
    }
    // Host bind-mounts are a local-only concept: a remote machine has no host
    // filesystem to bind. The cloud API has no field for them, so rather than
    // silently dropping them, reject up front. (Cloud persistent storage is a
    // separate, volume-based feature, not host mounts.) Published ports, by
    // contrast, ARE a cloud feature — the control plane allocates a node host
    // port for each guest port and routes ingress to it.
    if (config.mounts?.length) {
      throw new NotSupportedError(
        "host mounts are local-only and are not applied on the cloud target; " +
          "use cloud volumes for persistent storage instead.",
      );
    }
    const baseUrl = (
      conn.baseUrl ??
      process.env.SMOL_CLOUD_URL ??
      DEFAULT_CLOUD_URL
    ).replace(/\/+$/, "");
    const cloudConn: CloudConn = { baseUrl, apiKey };

    // smolfleet CreateMachineRequest (camelCase): source (tagged), nested
    // resources, network {mode}, autoStopSeconds, ttlSeconds. Optional numeric
    // fields are omitted when unset so the server applies its own defaults
    // (exactOptionalPropertyTypes forbids passing `undefined` explicitly).
    const createBody: CreateMachineRequest = {
      name: config.name ?? null,
      source: { type: "image", reference: config.image },
      resources: {
        ...(config.resources?.cpus !== undefined
          ? { cpus: config.resources.cpus }
          : {}),
        ...(config.resources?.memoryMb !== undefined
          ? { memoryMb: config.resources.memoryMb }
          : {}),
        diskGb: config.resources?.storageGb ?? null,
      },
      ...(config.resources?.allowCidrs?.length ||
      config.resources?.allowHosts?.length
        ? {
            network: {
              mode: "allowCidrs" as const,
              cidrs: config.resources.allowCidrs ?? [],
              hosts: config.resources.allowHosts ?? [],
            },
          }
        : config.resources?.network
          ? { network: { mode: "open" as const } }
          : {}),
      // Publish ports: supply only the guest port; the control plane allocates
      // the node host port (read it back from the machine info after start).
      // Publishing a port implies the virtio-net backend on the node.
      ...(config.ports?.length
        ? { ports: config.ports.map((p) => ({ port: p.guest })) }
        : {}),
      autoStopSeconds: config.autoStopSeconds ?? null,
      ttlSeconds: config.ttlSeconds ?? null,
    };
    const created = await cloudFetch<MachineInfo>(
      cloudConn,
      "POST",
      "/v1/machines",
      {
        json: createBody,
      },
    );
    const id: string = created.id;
    const name: string = created.name ?? config.name ?? id;
    // The machine now exists on the cloud. If start/readiness fails, delete it
    // before propagating — otherwise it leaks (and bills) as an orphan.
    try {
      // Best-effort start (cloud may auto-start; waitForReady is the gate).
      // A forkable golden boots with cloneable guest RAM (memfd) so it can later
      // be forked with Machine.fork (live-RAM CoW, RL rollouts).
      const startPath = `/v1/machines/${id}/start${config.forkable ? "?forkable=true" : ""}`;
      try {
        await cloudFetch(cloudConn, "POST", startPath);
      } catch {
        /* auto-start backends 4xx here — waitForReady decides readiness */
      }
      await waitForReady(cloudConn, id);
    } catch (e) {
      await cloudFetch(cloudConn, "DELETE", `/v1/machines/${id}`).catch(
        () => {},
      );
      throw e;
    }
    return new CloudTransport(cloudConn, name, id);
  }

  // Local embedded engine.
  const name = config.name ?? generateName();
  try {
    const inner = new (getNapiMachine())(toNativeConfig(name, config));
    // A forkable golden boots with memfd-backed guest RAM + a control socket so
    // it can be cloned with Machine.fork (local live-RAM fork).
    if (config.forkable) {
      await inner.startForkable();
    } else {
      await inner.start();
    }
    return new LocalTransport(inner);
  } catch (e) {
    throw wrapNativeError(e);
  }
}

/**
 * Attach to an EXISTING machine without creating a new one — for driving a
 * machine made elsewhere (another process, the console, the API).
 *  - local: re-opens a persisted machine by NAME, starting it if stopped.
 *  - cloud: looks up the machine by id (throws NOT_FOUND otherwise).
 */
export async function connectTransport(
  id: string,
  conn: ConnectOptions,
): Promise<Transport> {
  const apiKey = conn.apiKey ?? process.env.SMOL_CLOUD_TOKEN;
  const useCloud =
    conn.target === "cloud" || (conn.target !== "local" && !!apiKey);
  if (!useCloud) {
    // Local: start-or-reconnect to the named machine via the native engine.
    try {
      return new LocalTransport(getNapiMachine().connect(id));
    } catch (e) {
      throw wrapNativeError(e);
    }
  }
  if (!apiKey) {
    throw new InvalidConfigError(
      "connect requires an API key — pass { apiKey } or set SMOL_CLOUD_TOKEN (run 'smol login').",
    );
  }
  const baseUrl = (
    conn.baseUrl ??
    process.env.SMOL_CLOUD_URL ??
    DEFAULT_CLOUD_URL
  ).replace(/\/+$/, "");
  const cloudConn: CloudConn = { baseUrl, apiKey };
  const m = await cloudFetch<MachineInfo>(
    cloudConn,
    "GET",
    `/v1/machines/${id}`,
  );
  return new CloudTransport(cloudConn, m.name ?? id, m.id ?? id);
}
