/** Framework-aware fused rollout client for local training loops. */

import { createHash } from 'node:crypto';
import {
  closeSync,
  lstatSync,
  openSync,
  readFileSync,
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
  source: 'filesystem' | 'device';
  current: boolean;
  activeRequests: number;
  retiring: boolean;
}

export interface RolloutExecutorInfo {
  name: string;
  backend: 'vllm';
  endpoint: string;
  adapterRoot: string;
  deviceAdapterSocket?: string;
  fallbackPool?: string;
  maxConcurrentRequests: number;
  maxQueueDepth: number;
  requestTimeoutSecs: number;
  activeRequests: number;
  queuedRequests: number;
  accepting: boolean;
  policies: RolloutPolicyInfo[];
  capabilities: string[];
}

export interface EnsureRolloutExecutorOptions {
  endpoint: string;
  adapterRoot: string;
  deviceAdapterSocket?: string;
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

export interface RolloutCohort {
  id: string;
  size: number;
  maxWaitMs?: number;
}

export interface RolloutJob {
  idempotencyKey: string;
  policy: string;
  version?: string;
  prompts: Array<{ text: string } | { tokenIds: number[] }>;
  sampling: RolloutSampling;
  deadlineMs?: number;
  cohort?: RolloutCohort;
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
  cohort?: RolloutCohort;
}

export interface RolloutClientOptions {
  timeoutMs?: number;
  headers?: Record<string, string>;
  fetch?: typeof fetch;
  bearerToken?: string;
  forkEnvPath?: string;
  autoForkCohort?: boolean;
  autoForkCohortMaxWaitMs?: number;
}

const FORK_ENV_PATH = '/etc/smolvm/fork-env';
const ROLLOUT_URL_ENV = 'SMOLVM_ROLLOUT_URL';
const ROLLOUT_TOKEN_ENV = 'SMOLVM_ROLLOUT_TOKEN';
const ROLLOUT_EXECUTOR_ENV = 'SMOLVM_ROLLOUT_EXECUTOR';
const ROLLOUT_POLICY_ENV = 'SMOLVM_ROLLOUT_POLICY';
const ROLLOUT_LEASE_KEYS = [
  ROLLOUT_URL_ENV,
  ROLLOUT_TOKEN_ENV,
  ROLLOUT_EXECUTOR_ENV,
  ROLLOUT_POLICY_ENV,
] as const;

function readForkEnv(path: string): Record<string, string> {
  let contents: string;
  try {
    contents = readFileSync(path, 'utf8');
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return {};
    throw new Error(`cannot read smolvm fork assignment ${path}: ${String(error)}`);
  }
  const values: Record<string, string> = {};
  contents.split(/\r?\n/).forEach((line, index) => {
    if (line.length === 0 || line.startsWith('#')) return;
    const separator = line.indexOf('=');
    if (separator <= 0) {
      throw new Error(`invalid smolvm fork assignment at ${path}:${index + 1}`);
    }
    const key = line.slice(0, separator);
    if (Object.prototype.hasOwnProperty.call(values, key)) {
      throw new Error(`duplicate ${key} in smolvm fork assignment ${path}`);
    }
    values[key] = line.slice(separator + 1);
  });
  return values;
}

function leaseConfiguration(path: string) {
  const values = readForkEnv(path);
  for (const key of ROLLOUT_LEASE_KEYS) {
    if (process.env[key] !== undefined) values[key] = process.env[key] as string;
  }
  const missing = ROLLOUT_LEASE_KEYS.filter((key) => !values[key]);
  if (missing.length > 0) {
    throw new Error(`smolvm rollout lease configuration is unavailable: missing ${missing.join(', ')}`);
  }
  const url = values[ROLLOUT_URL_ENV].replace(/\/+$/, '');
  const executor = values[ROLLOUT_EXECUTOR_ENV];
  const suffix = `/rollout-executors/${executor}`;
  if (!url.endsWith(suffix)) {
    throw new Error(`${ROLLOUT_URL_ENV} does not match ${ROLLOUT_EXECUTOR_ENV}`);
  }
  const apiUrl = url.slice(0, -suffix.length);
  if (!apiUrl.startsWith('http://') && !apiUrl.startsWith('https://')) {
    throw new Error(`${ROLLOUT_URL_ENV} must be an HTTP URL`);
  }
  return {
    apiUrl,
    executor,
    bearerToken: values[ROLLOUT_TOKEN_ENV],
    leasePolicy: values[ROLLOUT_POLICY_ENV],
    assignment: values,
  };
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
  readonly leasePolicy: string | undefined;
  private readonly timeoutMs: number;
  private readonly headers: Record<string, string>;
  private readonly fetchFn: typeof fetch;
  private readonly autoForkCohort: boolean;
  private readonly autoForkCohortMaxWaitMs?: number;
  private readonly forkAssignment: Record<string, string>;
  private forkCohortRound = 0;
  private readonly forkCohorts = new Map<string, RolloutCohort>();

  constructor(apiUrl?: string, executor?: string, options: RolloutClientOptions = {}) {
    let bearerToken = options.bearerToken;
    let forkAssignment: Record<string, string> = {};
    let leasePolicy: string | undefined;
    if (apiUrl === undefined && executor === undefined) {
      const discovered = leaseConfiguration(options.forkEnvPath ?? FORK_ENV_PATH);
      apiUrl = discovered.apiUrl;
      executor = discovered.executor;
      leasePolicy = discovered.leasePolicy;
      forkAssignment = discovered.assignment;
      if (bearerToken !== undefined && bearerToken !== discovered.bearerToken) {
        throw new Error('bearerToken conflicts with the smolvm lease credential');
      }
      bearerToken = discovered.bearerToken;
    } else if (apiUrl === undefined || executor === undefined) {
      throw new TypeError('apiUrl and executor must be provided together');
    }
    const autoForkCohort = options.autoForkCohort ?? true;
    if (typeof autoForkCohort !== 'boolean') {
      throw new TypeError('autoForkCohort must be a boolean');
    }
    const maxWaitMs = options.autoForkCohortMaxWaitMs ?? 250;
    if (!Number.isInteger(maxWaitMs) || maxWaitMs < 1 || maxWaitMs > 60_000) {
      throw new RangeError('autoForkCohortMaxWaitMs must be between 1 and 60000');
    }
    this.apiUrl = apiUrl.replace(/\/+$/, '');
    this.executor = executor;
    this.leasePolicy = leasePolicy;
    this.timeoutMs = options.timeoutMs ?? 300_000;
    this.headers = {
      ...(options.headers ?? {}),
      ...(bearerToken === undefined ? {} : { authorization: `Bearer ${bearerToken}` }),
    };
    this.fetchFn = options.fetch ?? fetch;
    this.autoForkCohort = autoForkCohort;
    this.autoForkCohortMaxWaitMs = maxWaitMs;
    this.forkAssignment = forkAssignment;
  }

  private automaticForkCohort(idempotencyKey: string): RolloutCohort | undefined {
    if (!this.autoForkCohort) return undefined;
    const batchId = process.env.SMOLVM_FORK_BATCH_ID ?? this.forkAssignment.SMOLVM_FORK_BATCH_ID;
    const batchSizeText = process.env.SMOLVM_FORK_BATCH_SIZE
      ?? this.forkAssignment.SMOLVM_FORK_BATCH_SIZE;
    if (batchId === undefined && batchSizeText === undefined) return undefined;
    if (!batchId || batchSizeText === undefined) {
      throw new Error('SMOLVM_FORK_BATCH_ID and SMOLVM_FORK_BATCH_SIZE must be set together');
    }
    const batchSize = Number(batchSizeText);
    if (!Number.isInteger(batchSize) || batchSize < 2 || batchSize > 1024) {
      throw new RangeError('SMOLVM_FORK_BATCH_SIZE must be between 2 and 1024');
    }
    let groupIndex = 0;
    let groupSize = batchSize;
    if (batchSize > 256) {
      const forkIndexText = process.env.SMOLVM_FORK_INDEX ?? this.forkAssignment.SMOLVM_FORK_INDEX;
      const forkIndex = Number(forkIndexText);
      if (
        forkIndexText === undefined
        || !Number.isInteger(forkIndex)
        || forkIndex < 0
        || forkIndex >= batchSize
      ) {
        throw new RangeError('SMOLVM_FORK_INDEX is required and must be inside the fork batch');
      }
      groupIndex = Math.floor(forkIndex / 256);
      groupSize = Math.min(256, batchSize - groupIndex * 256);
    }
    const cached = this.forkCohorts.get(idempotencyKey);
    if (cached !== undefined) {
      this.forkCohorts.delete(idempotencyKey);
      this.forkCohorts.set(idempotencyKey, cached);
      return cached;
    }
    const round = this.forkCohortRound++;
    const digest = createHash('sha256')
      .update(`${batchId}\0${this.executor}\0${groupIndex}\0${round}`)
      .digest('hex');
    const cohort = optional({
      id: `fork-${digest.slice(0, 32)}`,
      size: groupSize,
      maxWaitMs: this.autoForkCohortMaxWaitMs,
    }) as RolloutCohort;
    this.forkCohorts.set(idempotencyKey, cohort);
    if (this.forkCohorts.size > 1024) {
      this.forkCohorts.delete(this.forkCohorts.keys().next().value as string);
    }
    return cohort;
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
      ...(options.deviceAdapterSocket === undefined
        ? {}
        : { deviceAdapterSocket: realpathSync(options.deviceAdapterSocket) }),
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
      deviceAdapterSocket: current.deviceAdapterSocket,
      fallbackPool: current.fallbackPool,
      maxConcurrentRequests: current.maxConcurrentRequests,
      maxQueueDepth: current.maxQueueDepth,
      requestTimeoutSecs: current.requestTimeoutSecs,
    };
    const expected = {
      backend: 'vllm',
      endpoint: desired.endpoint,
      adapterRoot: desired.adapterRoot,
      deviceAdapterSocket: options.deviceAdapterSocket === undefined
        ? undefined
        : desired.deviceAdapterSocket,
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

  async publishDevicePolicy(
    policy: string,
    version: string,
    tensorBundleToken: string | Uint8Array,
    retainPrevious = false,
  ): Promise<RolloutPolicyInfo> {
    let token: string;
    if (typeof tensorBundleToken === 'string') {
      if (!/^[0-9a-fA-F]{64}$/.test(tensorBundleToken)) {
        throw new Error('tensor bundle token must be exactly 32 hexadecimal bytes');
      }
      token = tensorBundleToken.toLowerCase();
    } else {
      if (tensorBundleToken.byteLength !== 32) {
        throw new Error('tensor bundle token must contain exactly 32 bytes');
      }
      token = Buffer.from(tensorBundleToken).toString('hex');
    }
    const path = `${this.executorPath}/device-policies`;
    const body = { policy, version, tensorBundleToken: token, retainPrevious };
    try {
      return await this.request('POST', path, body);
    } catch (error) {
      if (!(error instanceof RolloutError) || error.status !== 0) throw error;
      // The controller remembers the consumed token's digest, making an
      // ambiguous transport retry idempotent for this exact publication.
      return this.request('POST', path, body);
    }
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
    if (options.cohort !== undefined) {
      if (!options.cohort.id) throw new Error('cohort id cannot be empty');
      if (!Number.isInteger(options.cohort.size) || options.cohort.size < 1 || options.cohort.size > 256) {
        throw new RangeError('cohort size must be between 1 and 256');
      }
      if (
        options.cohort.maxWaitMs !== undefined
        && (!Number.isInteger(options.cohort.maxWaitMs)
          || options.cohort.maxWaitMs < 1
          || options.cohort.maxWaitMs > 60_000)
      ) {
        throw new RangeError('cohort maxWaitMs must be between 1 and 60000');
      }
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
      cohort: options.cohort,
    }) as RolloutJob;
  }

  generate(options: BuildRolloutJobOptions): Promise<RolloutGenerateResponse> {
    const cohort = options.cohort ?? this.automaticForkCohort(options.idempotencyKey);
    return this.request(
      'POST',
      `${this.executorPath}/generate`,
      this.job({ ...options, ...(cohort === undefined ? {} : { cohort }) }),
    );
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
