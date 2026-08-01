/** Framework-aware fused rollout client for local training loops. */

import { createHash } from 'node:crypto';
import {
  closeSync,
  lstatSync,
  openSync,
  readSync,
  readdirSync,
  realpathSync,
  statSync,
} from 'node:fs';
import { isAbsolute, relative, resolve, sep } from 'node:path';

export class RolloutError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(`${code}: ${message}`);
    this.name = 'RolloutError';
  }
}

export interface RolloutPolicyInfo {
  policy: string;
  version: string;
  adapterSha256: string;
  backendModel: string;
  current: boolean;
  activeRequests: number;
}

export interface RolloutExecutorInfo {
  name: string;
  backend: 'vllm';
  endpoint: string;
  adapterRoot: string;
  fallbackPool?: string;
  maxConcurrentRequests: number;
  maxQueueDepth: number;
  requestTimeoutSecs: number;
  activeRequests: number;
  queuedRequests: number;
  policies: RolloutPolicyInfo[];
  capabilities: string[];
}

export interface EnsureRolloutExecutorOptions {
  endpoint: string;
  adapterRoot: string;
  fallbackPool?: string;
  maxConcurrentRequests?: number;
  maxQueueDepth?: number;
  requestTimeoutSecs?: number;
}

export interface RolloutSampling {
  n?: number;
  maxTokens: number;
  temperature?: number;
  topP?: number;
  topK?: number;
  minP?: number;
  repetitionPenalty?: number;
  seed?: number;
  logprobs?: number;
  promptLogprobs?: number;
}

export type RolloutPrompt = string | readonly number[];

export interface RolloutJob {
  idempotencyKey: string;
  policy: string;
  version?: string;
  prompts: Array<{ text: string } | { tokenIds: number[] }>;
  sampling: RolloutSampling;
  deadlineMs?: number;
}

export interface RolloutCompletion {
  index: number;
  text: string;
  tokenIds?: number[];
  promptTokenIds?: number[];
  logprobs?: unknown;
  finishReason?: string;
  stopReason?: unknown;
}

export interface RolloutGenerateResponse {
  executor: string;
  policy: string;
  version: string;
  backendRequestId: string;
  choices: RolloutCompletion[];
  usage: { promptTokens: number; completionTokens: number; totalTokens: number };
  cached: boolean;
}

export interface RolloutBatchItem {
  idempotencyKey: string;
  response?: RolloutGenerateResponse;
  errorCode?: string;
  error?: string;
}

export interface BuildRolloutJobOptions {
  idempotencyKey: string;
  policy: string;
  version?: string;
  prompts: readonly RolloutPrompt[];
  sampling: RolloutSampling;
  deadlineMs?: number;
}

export interface RolloutClientOptions {
  timeoutMs?: number;
  headers?: Record<string, string>;
  fetch?: typeof fetch;
}

function lengthBytes(value: number): Buffer {
  const output = Buffer.allocUnsafe(8);
  output.writeBigUInt64LE(BigInt(value));
  return output;
}

/** Deterministically hash adapter file names, sizes, and contents. */
export function adapterSha256(directory: string): string {
  const root = realpathSync(directory);
  if (!statSync(root).isDirectory()) throw new Error('adapter path must be a directory');
  const files: string[] = [];
  const walk = (current: string) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const path = resolve(current, entry.name);
      if (entry.isSymbolicLink() || lstatSync(path).isSymbolicLink()) {
        throw new Error('adapter directories cannot contain symlinks');
      }
      if (entry.isDirectory()) walk(path);
      else if (entry.isFile()) files.push(path);
      else throw new Error('adapter directories may contain only regular files');
      if (files.length > 4096) throw new Error('adapter contains more than 4096 files');
    }
  };
  walk(root);
  if (files.length === 0) throw new Error('adapter directory contains no files');
  files.sort((left, right) =>
    Buffer.compare(
      Buffer.from(relative(root, left).split(sep).join('/')),
      Buffer.from(relative(root, right).split(sep).join('/')),
    ),
  );

  const hash = createHash('sha256');
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  let total = 0;
  for (const path of files) {
    const name = Buffer.from(relative(root, path).split(sep).join('/'));
    const size = statSync(path).size;
    total += size;
    if (total > 32 * 1024 * 1024 * 1024) throw new Error('adapter exceeds 32 GiB');
    hash.update(lengthBytes(name.length));
    hash.update(name);
    hash.update(lengthBytes(size));
    const fd = openSync(path, 'r');
    try {
      let offset = 0;
      while (offset < size) {
        const count = readSync(fd, buffer, 0, Math.min(buffer.length, size - offset), offset);
        if (count === 0) throw new Error(`adapter file changed while hashing: ${path}`);
        hash.update(buffer.subarray(0, count));
        offset += count;
      }
    } finally {
      closeSync(fd);
    }
  }
  return hash.digest('hex');
}

function segment(value: string): string {
  return encodeURIComponent(value);
}

function optional<T extends object>(value: T): T {
  return Object.fromEntries(Object.entries(value).filter(([, item]) => item !== undefined)) as T;
}

/** Client for publishing LoRA versions and generating fused policy cohorts. */
export class RolloutClient {
  readonly apiUrl: string;
  readonly executor: string;
  private readonly timeoutMs: number;
  private readonly headers: Record<string, string>;
  private readonly fetchFn: typeof fetch;

  constructor(apiUrl: string, executor: string, options: RolloutClientOptions = {}) {
    this.apiUrl = apiUrl.replace(/\/+$/, '');
    this.executor = executor;
    this.timeoutMs = options.timeoutMs ?? 300_000;
    this.headers = options.headers ?? {};
    this.fetchFn = options.fetch ?? fetch;
  }

  private get executorPath(): string {
    return `/rollout-executors/${segment(this.executor)}`;
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const response = await this.fetchFn(`${this.apiUrl}${path}`, {
        method,
        headers: { ...this.headers, 'content-type': 'application/json' },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
        signal: controller.signal,
      });
      const text = await response.text();
      let payload: unknown;
      try {
        payload = text.length === 0 ? undefined : JSON.parse(text);
      } catch {
        payload = undefined;
      }
      if (!response.ok) {
        const error = payload as { code?: unknown; error?: unknown } | undefined;
        throw new RolloutError(
          response.status,
          typeof error?.code === 'string' ? error.code : 'HTTP_ERROR',
          typeof error?.error === 'string' ? error.error : text || response.statusText,
        );
      }
      return payload as T;
    } catch (error) {
      if (error instanceof RolloutError) throw error;
      const message = error instanceof Error ? error.message : String(error);
      throw new RolloutError(0, 'UNAVAILABLE', message);
    } finally {
      clearTimeout(timer);
    }
  }

  async ensureVllmExecutor(options: EnsureRolloutExecutorOptions): Promise<RolloutExecutorInfo> {
    const desired = {
      name: this.executor,
      backend: 'vllm',
      endpoint: options.endpoint,
      adapterRoot: realpathSync(options.adapterRoot),
      maxConcurrentRequests: options.maxConcurrentRequests ?? 32,
      maxQueueDepth: options.maxQueueDepth ?? 256,
      requestTimeoutSecs: options.requestTimeoutSecs ?? 300,
      ...(options.fallbackPool === undefined ? {} : { fallbackPool: options.fallbackPool }),
    };
    try {
      return await this.request<RolloutExecutorInfo>('POST', '/rollout-executors', desired);
    } catch (error) {
      if (!(error instanceof RolloutError) || error.status !== 409) throw error;
    }
    const current = await this.info();
    const comparable = {
      backend: current.backend,
      endpoint: current.endpoint,
      adapterRoot: current.adapterRoot,
      fallbackPool: current.fallbackPool,
      maxConcurrentRequests: current.maxConcurrentRequests,
      maxQueueDepth: current.maxQueueDepth,
      requestTimeoutSecs: current.requestTimeoutSecs,
    };
    const expected = {
      backend: 'vllm',
      endpoint: desired.endpoint,
      adapterRoot: desired.adapterRoot,
      fallbackPool: options.fallbackPool,
      maxConcurrentRequests: desired.maxConcurrentRequests,
      maxQueueDepth: desired.maxQueueDepth,
      requestTimeoutSecs: desired.requestTimeoutSecs,
    };
    if (JSON.stringify(comparable) !== JSON.stringify(expected)) {
      throw new RolloutError(
        409,
        'CONFLICT',
        `executor ${JSON.stringify(this.executor)} exists with different configuration`,
      );
    }
    return current;
  }

  info(): Promise<RolloutExecutorInfo> {
    return this.request('GET', this.executorPath);
  }

  async publishPolicy(
    policy: string,
    version: string,
    adapterDirectory: string,
    retainPrevious = false,
  ): Promise<RolloutPolicyInfo> {
    const info = await this.info();
    const root = realpathSync(info.adapterRoot);
    const adapter = realpathSync(adapterDirectory);
    const adapterPath = relative(root, adapter);
    if (
      adapterPath.length === 0 ||
      adapterPath === '..' ||
      adapterPath.startsWith(`..${sep}`) ||
      isAbsolute(adapterPath)
    ) {
      throw new Error('adapter must be beneath the executor adapter root');
    }
    return this.request('POST', `${this.executorPath}/policies`, {
      policy,
      version,
      adapterPath: adapterPath.split(sep).join('/'),
      adapterSha256: adapterSha256(adapter),
      retainPrevious,
    });
  }

  async retirePolicy(policy: string, version: string): Promise<void> {
    await this.request(
      'DELETE',
      `${this.executorPath}/policies/${segment(policy)}/${segment(version)}`,
    );
  }

  job(options: BuildRolloutJobOptions): RolloutJob {
    if (options.prompts.length === 0) throw new Error('at least one prompt is required');
    const text = typeof options.prompts[0] === 'string';
    if (!options.prompts.every((prompt) => (typeof prompt === 'string') === text)) {
      throw new Error('one request cannot mix text and token prompts');
    }
    return optional({
      idempotencyKey: options.idempotencyKey,
      policy: options.policy,
      version: options.version,
      prompts: options.prompts.map((prompt) =>
        typeof prompt === 'string' ? { text: prompt } : { tokenIds: [...prompt] },
      ),
      sampling: optional({ ...options.sampling, n: options.sampling.n ?? 1 }),
      deadlineMs: options.deadlineMs,
    }) as RolloutJob;
  }

  generate(options: BuildRolloutJobOptions): Promise<RolloutGenerateResponse> {
    return this.request('POST', `${this.executorPath}/generate`, this.job(options));
  }

  async generateBatch(jobs: readonly RolloutJob[]): Promise<RolloutBatchItem[]> {
    const response = await this.request<{ jobs: RolloutBatchItem[] }>(
      'POST',
      `${this.executorPath}/batches`,
      { jobs },
    );
    return response.jobs;
  }

  async close(): Promise<void> {
    await this.request('DELETE', this.executorPath);
  }
}
