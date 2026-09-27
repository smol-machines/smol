/** Managed agent sessions on smol cloud. The repository/workspace is supplied by the caller. */
import { InvalidConfigError, SmolError } from "./errors";
import { Machine } from "./machine";
import { CLOUD_START_TIMEOUT_MS, cloudFetch, resolveCloudConnection, type CloudConn } from "./transport";
import type { ConnectOptions } from "./types";
import type { components } from "./generated/smolfleet";

type Schemas = components["schemas"];
export interface CreateAgentOptions extends Omit<Schemas["CreateAgentRequest"], "harness"> {
  harness?: "claude-code" | "codex" | "opencode" | "command";
}

export type AgentTurn = Schemas["AgentTurnInfo"];
export type AgentInfo = Schemas["AgentInfo"];
export type AgentSummary = Schemas["AgentSummary"];
export type AgentPage = Schemas["AgentListPage"];
export interface SendTurnOptions {
  env?: Record<string, string>;
  timeoutSeconds?: number;
  idempotencyKey?: string;
}
export type AgentStreamEvent =
  | { type: "event"; id: number; data: Record<string, unknown> }
  | { type: "done"; turn: AgentTurn };

function path(name: string): string {
  if (!name || name.includes("/")) throw new InvalidConfigError("invalid agent session name");
  return `/v1/agents/${encodeURIComponent(name)}`;
}

export class AgentSession {
  private constructor(readonly name: string, private readonly conn: CloudConn) {}

  static async create(options: CreateAgentOptions, connect: ConnectOptions = {}): Promise<AgentSession> {
    const conn = resolveCloudConnection(connect);
    const info = await cloudFetch<AgentInfo>(conn, "POST", "/v1/agents", { json: options });
    return new AgentSession(info.name, conn);
  }

  static async connect(name: string, connect: ConnectOptions = {}): Promise<AgentSession> {
    const conn = resolveCloudConnection(connect);
    await cloudFetch<AgentInfo>(conn, "GET", path(name));
    return new AgentSession(name, conn);
  }

  static list(connect: ConnectOptions = {}, options: { after?: string; limit?: number } = {}): Promise<AgentPage> {
    const query = new URLSearchParams();
    if (options.after) query.set("after", options.after);
    if (options.limit !== undefined) query.set("limit", String(options.limit));
    const suffix = query.toString() ? `?${query}` : "";
    return cloudFetch<AgentPage>(resolveCloudConnection(connect), "GET", `/v1/agents${suffix}`);
  }

  info(): Promise<AgentInfo> {
    return cloudFetch<AgentInfo>(this.conn, "GET", path(this.name));
  }

  /** Attach to the underlying machine to stage files or inspect the workspace. */
  async machine(): Promise<Machine> {
    const id = (await this.info()).machineId;
    if (!id) throw new SmolError("INVALID_STATE", "agent session has no machine yet");
    return Machine.connect(id, { target: "cloud", baseUrl: this.conn.baseUrl, apiKey: this.conn.apiKey });
  }

  async send(prompt: string, options: SendTurnOptions = {}): Promise<number> {
    const { idempotencyKey, ...body } = options;
    const response = await cloudFetch<{ turn: number }>(this.conn, "POST", `${path(this.name)}/turns`, {
      json: { prompt, ...body },
      ...(idempotencyKey ? { headers: { "Idempotency-Key": idempotencyKey } } : {}),
    });
    return response.turn;
  }

  /** Replay events after `after`, then follow the turn until its terminal event. */
  async *events(turn: number, options: { after?: number; signal?: AbortSignal } = {}): AsyncGenerator<AgentStreamEvent> {
    const query = options.after === undefined ? "" : `?after=${options.after}`;
    const endpoint = `${path(this.name)}/turns/${turn}/events${query}`;
    const response = await fetch(`${this.conn.baseUrl}${endpoint}`, {
      headers: { authorization: `Bearer ${this.conn.apiKey}`, accept: "text/event-stream" },
      ...(options.signal ? { signal: options.signal } : {}),
      redirect: "error",
    });
    if (!response.ok) throw new SmolError("SMOLVM_ERROR", `cloud GET ${endpoint} → ${response.status}: ${await response.text()}`);
    if (!response.body) throw new SmolError("CONNECTION", "agent event stream has no body");
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let done = false;
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        buffer = (buffer + decoder.decode(chunk.value, { stream: true })).replace(/\r\n/g, "\n");
        if (buffer.length > 2 * 1024 * 1024) throw new SmolError("SMOLVM_ERROR", "agent event frame is too large");
        while (buffer.includes("\n\n")) {
          const end = buffer.indexOf("\n\n");
          const frame = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          let kind = "";
          let id = 0;
          const data: string[] = [];
          for (const line of frame.split("\n")) {
            if (line.startsWith("event:")) kind = line.slice(6).trim();
            else if (line.startsWith("id:")) id = Number(line.slice(3).trim());
            else if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
          }
          const payload = data.join("\n");
          if (kind === "event") yield { type: "event", id, data: JSON.parse(payload) as Record<string, unknown> };
          else if (kind === "done") {
            done = true;
            yield { type: "done", turn: JSON.parse(payload) as AgentTurn };
            return;
          } else if (kind === "error") throw new SmolError("SMOLVM_ERROR", payload);
        }
      }
      if (!done) throw new SmolError("CONNECTION", "agent event stream ended before the turn finished; reconnect with after");
    } finally {
      await reader.cancel().catch(() => {});
    }
  }

  async cancel(turn: number): Promise<void> {
    await cloudFetch(this.conn, "POST", `${path(this.name)}/turns/${turn}/cancel`, { timeoutMs: CLOUD_START_TIMEOUT_MS });
  }
  rewind(turn: number): Promise<AgentInfo> {
    return cloudFetch<AgentInfo>(this.conn, "POST", `${path(this.name)}/rewind`, { json: { turn }, timeoutMs: CLOUD_START_TIMEOUT_MS });
  }
  async branch(turn: number, name: string): Promise<AgentSession> {
    const options = { json: { turn, name }, timeoutMs: CLOUD_START_TIMEOUT_MS };
    let info: AgentInfo;
    try {
      info = await cloudFetch<AgentInfo>(this.conn, "POST", `${path(this.name)}/branch`, options);
    } catch (error) {
      if (!(error instanceof SmolError) || error.code !== "NOT_FOUND") throw error;
      info = await cloudFetch<AgentInfo>(this.conn, "POST", `${path(this.name)}/fork`, options);
    }
    return new AgentSession(info.name, this.conn);
  }
  /** Compatibility alias for branch. */
  fork(turn: number, name: string): Promise<AgentSession> { return this.branch(turn, name); }
  async pause(): Promise<void> { await cloudFetch(this.conn, "POST", `${path(this.name)}/pause`, { timeoutMs: CLOUD_START_TIMEOUT_MS }); }
  async resume(): Promise<void> { await cloudFetch(this.conn, "POST", `${path(this.name)}/resume`, { timeoutMs: CLOUD_START_TIMEOUT_MS }); }
  async delete(): Promise<void> { await cloudFetch(this.conn, "DELETE", path(this.name), { timeoutMs: CLOUD_START_TIMEOUT_MS }); }
}
