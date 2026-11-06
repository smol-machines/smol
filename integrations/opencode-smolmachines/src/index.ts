import type { Plugin, PluginModule } from "@opencode-ai/plugin"
import { tool } from "@opencode-ai/plugin"
import { Machine } from "smolmachines"

import { DEFAULT_TIMEOUT_MS, Sandbox, type MachineApi, type SandboxOptions } from "./sandbox.js"

export type { SandboxOptions } from "./sandbox.js"

const DESCRIPTION = `Executes a shell command inside this project's smol machine: an isolated Linux microVM with its own kernel.

The project directory is mounted at the same path, so files you change with other tools are visible here and vice versa. Host paths outside configured mounts and host environment variables are not forwarded by default. Files in the mounted project, including any credentials there, are visible; network access follows the sandbox's policy.

- Use \`workdir\` instead of \`cd\`; it must be inside the project.
- Chain dependent commands with \`&&\` in one call.
- Do not use this tool to read, write, edit or search files; use the dedicated tools.
- Packages you install persist in the machine between sessions.`

const smolMachines: MachineApi = {
  create: (config) => Machine.create(config as Parameters<typeof Machine.create>[0]),
  connect: (name) => Machine.connect(name, { target: "local" } as Parameters<typeof Machine.connect>[1]),
  list: async () => (await Machine.list({ target: "local" })).map(({ name, labels }) => ({ name, labels })),
}

/** The first word of each command, as OpenCode's own shell tool keys "always allow" rules. */
function alwaysPattern(command: string): string {
  const first = command.trim().split(/\s+/)[0] ?? command
  return `${first} *`
}

const server: Plugin = async ({ worktree, directory }, options) => {
  const sandbox = new Sandbox(worktree && worktree !== "/" ? worktree : directory, (options ?? {}) as SandboxOptions, smolMachines)

  return {
    tool: {
      // Same ID as the built-in shell tool, which a plugin tool replaces, so agents, prompts and
      // saved permissions keep working unchanged.
      bash: tool({
        description: DESCRIPTION,
        args: {
          command: tool.schema.string().describe("The command to execute"),
          timeout: tool.schema.number().int().positive().optional().describe("Optional timeout in milliseconds"),
          workdir: tool.schema
            .string()
            .optional()
            .describe("The working directory to run the command in. Defaults to the current directory."),
          description: tool.schema
            .string()
            .optional()
            .describe("Clear, concise description of what this command does in 5-10 words"),
        },
        async execute(args, ctx) {
          await ctx.ask({
            permission: "bash",
            patterns: [args.command],
            always: [alwaysPattern(args.command)],
            metadata: { command: args.command, sandbox: "smolmachines" },
          })
          const result = await sandbox.run({
            command: args.command,
            directory: ctx.directory,
            workdir: args.workdir,
            timeoutMs: args.timeout ?? DEFAULT_TIMEOUT_MS,
            signal: ctx.abort,
            onStart: () => ctx.metadata({ title: "Starting sandbox (first run pulls the image)" }),
          })
          ctx.metadata({ title: args.description ?? args.command, metadata: result.metadata })
          return result
        },
      }),
    },
    dispose: () => sandbox.dispose(),
  }
}

const plugin: PluginModule = { id: "smolmachines", server }
export default plugin
