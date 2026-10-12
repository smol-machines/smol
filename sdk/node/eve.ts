/**
 * smolmachines as an eve sandbox provider.
 *
 * ```ts title="agent/sandbox.ts"
 * import { defineSandbox } from "eve/sandbox";
 * import { SmolmachinesSandbox } from "smolmachines/eve";
 *
 * export const environment = SmolmachinesSandbox.environment({
 *   prepare: async (sandbox) => {
 *     await sandbox.run({ command: "pip install pandas" });
 *   },
 * });
 * export default defineSandbox(() => environment.open({ networkPolicy: { allow: ["pypi.org"] } }));
 * ```
 *
 * eve prepares the environment once: a machine boots from the image, receives
 * the workspace and skills, runs `prepare`, and is saved as a checkpoint. Each
 * eve session then restores its own machine from that checkpoint, with the
 * session's network policy applied before it first boots. Machines are local
 * microVMs (Linux with KVM, or macOS on Apple Silicon).
 *
 * smolmachines loads a native addon, so list it in the agent's
 * `build.externalDependencies`. Needs eve >= 0.67.2 on Node 24, as eve does.
 */

import { createHash, randomUUID } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { mkdir, readdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import type { SandboxNetworkPolicy, SandboxSession } from "eve/sandbox" with { "resolution-mode": "import" };
import type {
  SandboxProvider,
  SandboxProviderResources,
  SandboxProviderResourceTree,
} from "eve/sandbox/provider" with { "resolution-mode": "import" };
import { createSandboxSession, type SmolSandboxSession } from "./ai-sdk";
import { InvalidConfigError, NotSupportedError } from "./errors";
import { Machine } from "./machine";
import type { ConnectOptions, NetworkPolicy } from "./types";

/** Options for {@link SmolmachinesSandbox.environment}. */
export interface SmolmachinesEnvironmentOptions {
  /** OCI image the environment starts from. Default: eve's own sandbox image,
   *  `ghcr.io/vercel/eve:<installed eve version>` (or `EVE_SANDBOX_IMAGE_TAG`). */
  readonly image?: string;
  /** vCPUs per machine. Default: 1. */
  readonly cpus?: number;
  /** Memory per machine in MiB. Default: 1024. */
  readonly memoryMiB?: number;
  /** Environment variables for every sandbox command. */
  readonly env?: Readonly<Record<string, string>>;
  /** Setup every session inherits, captured in the prepared checkpoint. Runs
   *  with open egress. */
  readonly prepare?: (sandbox: SandboxSession) => Promise<void> | void;
}

/** Options for one session's machine, passed to `environment.open()`. */
export interface SmolmachinesOpenOptions {
  /** Outbound network policy for the session's machine. Default: `"allow-all"`.
   *  `allow` takes host names and `*.` subdomain wildcards, and `subnets.allow`
   *  CIDRs; per-host request rules and `subnets.deny` are not supported. */
  readonly networkPolicy?: SandboxNetworkPolicy;
}

type PreparedArtifact = {
  readonly version: 1;
  readonly checkpoint: string;
  readonly image: string;
  readonly home: string;
};

type SessionState = {
  readonly version: 1;
  readonly machine: string;
};

/** The user every sandbox command runs as, as with eve's other providers. */
const SANDBOX_USER = "vercel-sandbox";
const WORKSPACE = "/workspace";
const RESOURCES_ROOT = "/eve/resources";
/** Machines outlive the eve process; eve's lifecycle hooks stop and delete them. */
const LOCAL: ConnectOptions = { target: "local", handleSignals: false };

const BASE_SETUP = `set -eu
if ! id -u ${SANDBOX_USER} >/dev/null 2>&1; then
  if command -v useradd >/dev/null 2>&1; then useradd -m -s /bin/bash ${SANDBOX_USER} 2>/dev/null || useradd -m ${SANDBOX_USER}
  else adduser -D ${SANDBOX_USER}
  fi
fi
mkdir -p ${WORKSPACE}
chown ${SANDBOX_USER}: ${WORKSPACE}
awk -F: '$1 == "${SANDBOX_USER}" { print $6 }' /etc/passwd`;

// A session machine lives as long as the eve process that started it. eve
// stops it cleanly on a production shutdown, but `eve dev` restarts and
// crashes end it with the process; flushing the guest's writes within a
// second, instead of Linux's default 30, keeps what the agent last wrote.
const FAST_WRITEBACK = `for knob in dirty_expire_centisecs dirty_writeback_centisecs; do echo 100 > /proc/sys/vm/$knob; done`;

const hash = (value: unknown): string => createHash("sha256").update(JSON.stringify(value)).digest("hex");

/** eve's default sandbox image for the installed eve version. */
function defaultImage(): string {
  const tag = process.env.EVE_SANDBOX_IMAGE_TAG?.trim();
  if (tag) return `ghcr.io/vercel/eve:${tag}`;
  // eve's `exports` hide its package.json; find it from an exported entry.
  let dir = dirname(require.resolve("eve/sandbox/provider"));
  while (!existsSync(join(dir, "package.json")) || !/"name":\s*"eve"/.test(readFileSync(join(dir, "package.json"), "utf8"))) {
    const parent = dirname(dir);
    if (parent === dir) throw new InvalidConfigError("could not find eve's version; set `image` or EVE_SANDBOX_IMAGE_TAG");
    dir = parent;
  }
  const version = (JSON.parse(readFileSync(join(dir, "package.json"), "utf8")) as { version: string }).version;
  return `ghcr.io/vercel/eve:${version.replace(/\+.*$/, "")}`;
}

/** Map eve's (Vercel-shaped) network policy onto a machine egress policy. */
export function toMachineNetworkPolicy(policy: SandboxNetworkPolicy | undefined): NetworkPolicy {
  if (policy === undefined || policy === "allow-all") return "allow-all";
  if (policy === "deny-all") return "deny-all";
  if (typeof policy !== "object" || policy === null) {
    throw new InvalidConfigError(`network policy must be "allow-all", "deny-all" or { allow, subnets }`);
  }
  if (policy.subnets?.deny?.length) {
    throw new NotSupportedError("smolmachines network policies do not support subnets.deny; list only what to allow");
  }
  const allow = policy.allow ?? [];
  let hosts: string[];
  if (Array.isArray(allow)) {
    hosts = [...allow];
  } else {
    hosts = Object.entries(allow).map(([host, rules]) => {
      if (rules.length > 0) {
        throw new NotSupportedError(`smolmachines network policies do not support request rules (for ${host}); allow the host with []`);
      }
      return host;
    });
  }
  if (hosts.includes("*")) return "allow-all";
  return { allowHosts: hosts, allowCidrs: [...(policy.subnets?.allow ?? [])] };
}

/** The final path of every workspace and skills file, with its content. */
async function resourceFiles(
  resources: SandboxProviderResources,
  home: string,
): Promise<Array<{ path: string; content: Uint8Array }>> {
  const target = (tree: SandboxProviderResourceTree) => tree.targetPath.replace(/^\$HOME(?=\/|$)/, home);
  const trees = [resources.workspace, resources.skills].filter((t): t is SandboxProviderResourceTree => t !== undefined);
  const out: Array<{ path: string; content: Uint8Array }> = [];
  const source = resources.source;
  for (const tree of trees) {
    if (source.kind === "materialized") {
      // Compiled trees sit on the host under the path their mount would use.
      const dir = join(source.path, relative(RESOURCES_ROOT, tree.mountPath));
      for (const file of await walk(dir)) {
        out.push({ path: `${target(tree)}/${relative(dir, file)}`, content: await readFile(file) });
      }
    } else {
      for (const file of tree.files) {
        const content = typeof file.content === "string" ? new TextEncoder().encode(file.content) : file.content;
        out.push({ path: `${target(tree)}/${file.relativePath}`, content });
      }
    }
  }
  return out;
}

async function walk(dir: string): Promise<string[]> {
  if (!existsSync(dir)) return [];
  const entries = await readdir(dir, { withFileTypes: true });
  const nested = await Promise.all(
    entries.map((e) => (e.isDirectory() ? walk(join(dir, e.name)) : Promise.resolve(e.isFile() ? [join(dir, e.name)] : []))),
  );
  return nested.flat();
}

async function check(machine: Machine, what: string, command: string, user = "root"): Promise<string> {
  const r = await machine.exec(["sh", "-c", command], { user });
  if (r.exitCode !== 0) throw new Error(`${what} failed: ${(r.stderr || r.stdout).trim()}`);
  return r.stdout;
}

function sessionFor(machine: Machine, home: string, env: Readonly<Record<string, string>> | undefined): SmolSandboxSession {
  return createSandboxSession(machine, {
    root: WORKSPACE,
    user: SANDBOX_USER,
    env: { HOME: home, ...env },
  });
}

/** Handles already open in this process, so a resume reuses the live one. */
const openHandles = new Map<string, ReturnType<typeof handleFor>>();

function handleFor(machine: Machine, home: string, env: Readonly<Record<string, string>> | undefined) {
  const forget = () => openHandles.delete(machine.name);
  const stop = async () => {
    forget();
    await machine.stop();
  };
  const destroy = async () => {
    forget();
    await machine.delete();
  };
  const handle = {
    sandbox: sessionFor(machine, home, env),
    onRuntimeShutdown: stop,
    // Eve 0.71 renamed these hooks; keep the earlier names so already-built
    // agents on 0.67 continue to stop and delete their VMs as before.
    onSandboxStop: stop,
    onSandboxDelete: destroy,
    onSessionStop: stop,
    onSessionDelete: destroy,
  };
  openHandles.set(machine.name, handle);
  return handle;
}

type Define = typeof import("eve/sandbox/provider", { with: { "resolution-mode": "import" } }).defineSandboxProvider;

function loadEve(): Define {
  try {
    return (require("eve/sandbox/provider") as { defineSandboxProvider: Define }).defineSandboxProvider;
  } catch (e) {
    throw new Error(`smolmachines/eve needs the eve package (Node 24+): ${(e as Error).message}`);
  }
}

const provider = loadEve()<
  SmolmachinesEnvironmentOptions | undefined,
  SmolmachinesOpenOptions | undefined,
  PreparedArtifact,
  SessionState,
  SmolSandboxSession
>({
  name: "smolmachines",
  stateProtocolVersion: 1,
  environment(options) {
    const image = options?.image ?? defaultImage();
    const cpus = options?.cpus ?? 1;
    const memoryMb = options?.memoryMiB ?? 1024;
    const env = options?.env;

    return {
      // Eve 0.76 calls this when a durable session completes or expires. Earlier
      // versions have no such hook; the spread keeps their type surface intact.
      ...{
        async onSessionEnd(_ctx: unknown, _artifact: Readonly<PreparedArtifact>, state: Readonly<SessionState>) {
          const live = openHandles.get(state.machine);
          if (live) {
            await live.onSandboxDelete();
            return;
          }
          const exists = async () => (await Machine.list(LOCAL)).some(({ name }) => name === state.machine);
          if (!(await exists())) return;
          try {
            const machine = await Machine.connect(state.machine, LOCAL);
            await machine.delete();
          } catch (error) {
            // A concurrent cleanup may have removed it after the list.
            if (!(await exists())) return;
            throw error;
          }
        },
      },
      async prepare(ctx) {
        const r = ctx.resources;
        const key = hash({
          image,
          cpus,
          memoryMb,
          env,
          resources: [r.source.kind === "none" ? null : r.source.key, r.workspace?.key, r.skills?.key],
          source: ctx.sourceRevision,
        }).slice(0, 32);
        const dir = join(ctx.storagePath, "smolmachines");
        const checkpoint = join(dir, `${key}.smolcheckpoint`);
        if (existsSync(checkpoint)) {
          ctx.log?.("reusing the prepared checkpoint");
          const saved = await readFile(`${checkpoint}.json`, "utf8").then((t) => JSON.parse(t) as PreparedArtifact).catch(() => null);
          if (saved?.version === 1) return saved;
        }
        await mkdir(dir, { recursive: true });

        ctx.log?.(`creating a machine from ${image}`);
        const machine = await Machine.create(
          {
            name: `eve-prepare-${key.slice(0, 12)}-${randomUUID().slice(0, 8)}`,
            image,
            branchable: true,
            resources: { cpus, memoryMb, network: true, networkBackend: "virtio-net" },
          },
          LOCAL,
        );
        // Checkpoint outputs must end in .smolcheckpoint; rename once complete.
        const partial = join(dir, `${key}.${randomUUID()}.partial.smolcheckpoint`);
        try {
          const home = (await check(machine, "sandbox user setup", BASE_SETUP)).trim() || `/home/${SANDBOX_USER}`;

          const files = await resourceFiles(r, home);
          if (files.length > 0) {
            ctx.log?.(`writing ${files.length} workspace and skill files`);
            const dirs = [...new Set(files.map((f) => f.path.slice(0, f.path.lastIndexOf("/"))))];
            await check(machine, "create resource directories", `mkdir -p ${dirs.map(shellQuote).join(" ")}`, SANDBOX_USER);
            for (const file of files) await machine.writeFile(file.path, Buffer.from(file.content));
            await check(machine, "hand resources to the sandbox user", `chown -R ${SANDBOX_USER}: ${WORKSPACE} ${shellQuote(`${home}/.agents`)} 2>/dev/null || chown -R ${SANDBOX_USER}: ${WORKSPACE}`);
          }

          if (options?.prepare) {
            ctx.log?.("running sandbox preparation");
            await options.prepare(sessionFor(machine, home, env));
          }

          ctx.log?.("saving the prepared checkpoint");
          await machine.checkpoint(partial);
          await rename(partial, checkpoint);
          const artifact: PreparedArtifact = { version: 1, checkpoint, image, home };
          await writeFile(`${checkpoint}.json`, JSON.stringify(artifact));
          return artifact;
        } finally {
          await rm(partial, { force: true }).catch(() => {});
          await machine.delete().catch(() => {});
        }
      },

      async start(ctx, open, artifact) {
        if (!existsSync(artifact.checkpoint)) {
          throw new Error(`the prepared smolmachines checkpoint ${artifact.checkpoint} is missing; rebuild so eve prepares it again`);
        }
        const policy = toMachineNetworkPolicy(open?.networkPolicy);
        const name = `eve-${hash([ctx.session.id, artifact.checkpoint]).slice(0, 24)}`;
        // A start that failed after restoring leaves its machine behind.
        const stale = await Machine.connect(name, LOCAL).catch(() => null);
        if (stale) await stale.delete().catch(() => {});
        const machine = await Machine.restoreCheckpoint(artifact.checkpoint, name, LOCAL, { networkPolicy: policy });
        await check(machine, "tune guest writeback", FAST_WRITEBACK);
        return { handle: handleFor(machine, artifact.home, env), state: { version: 1, machine: name } };
      },

      async resume(_ctx, artifact, state) {
        const live = openHandles.get(state.machine);
        if (live) return live;
        let machine: Machine;
        try {
          machine = await Machine.connect(state.machine, LOCAL);
        } catch (e) {
          throw new Error(`smolmachines machine "${state.machine}" is no longer available to resume: ${(e as Error).message}`);
        }
        await check(machine, "tune guest writeback", FAST_WRITEBACK);
        return handleFor(machine, artifact.home, env);
      },
    };
  },
});

const shellQuote = (value: string): string => `'${value.replace(/'/g, `'\\''`)}'`;

/** The smolmachines eve sandbox provider. */
export const SmolmachinesSandbox: SandboxProvider<
  SmolmachinesEnvironmentOptions | undefined,
  SmolmachinesOpenOptions | undefined,
  SmolSandboxSession
> & {
  /** An environment from an existing OCI image. */
  image(
    image: string,
    options?: Omit<SmolmachinesEnvironmentOptions, "image">,
  ): ReturnType<typeof provider.environment>;
} = {
  ...provider,
  environment: provider.environment.bind(provider),
  image: (image, options = {}) => provider.environment({ ...options, image }),
};
