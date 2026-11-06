/** Run with `bun run test:live` on macOS Apple Silicon or a Linux KVM host. */
import { expect, test } from "bun:test"
import { mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import { Machine } from "smolmachines"
import plugin from "../src/index"
import { Sandbox, type MachineApi } from "../src/sandbox"

const live = process.env.SMOL_OPENCODE_LIVE === "1" ? test : test.skip

live("a project resumes its VM but a changed policy never boots the old one", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "opencode-smol-live-"))
  const conn = { target: "local", handleSignals: false, waitForPorts: false } as const
  const api: MachineApi = {
    create: (config) => Machine.create(config, conn),
    connect: (name) => Machine.connect(name, conn),
    list: async () => (await Machine.list(conn)).map(({ name, labels }) => ({ name, labels })),
  }
  const first = new Sandbox(root, { image: null, network: false }, api)
  const changed = new Sandbox(root, { image: null, network: true }, api)
  try {
    const out = await first.run({ command: "printf isolated > guest.txt; cat guest.txt", directory: root })
    expect(out.output).toBe("isolated")
    expect(await readFile(path.join(root, "guest.txt"), "utf8")).toBe("isolated")
    await first.dispose()
    const listed = await Machine.list(conn)
    expect(listed.find((machine) => machine.name === first.name)?.state).toBe("stopped")

    const resumed = new Sandbox(root, { image: null, network: false }, api)
    expect((await resumed.run({ command: "cat guest.txt", directory: root })).output).toBe("isolated")
    await resumed.dispose()
    expect(changed.name).not.toBe(first.name)
    expect((await changed.run({ command: "cat guest.txt", directory: root })).output).toBe("isolated")
    expect((await Machine.list(conn)).find((machine) => machine.name === first.name)?.state).toBe("stopped")
  } finally {
    await changed.dispose()
    // These names are unique to this temp project; remove both records and mounts.
    for (const name of [first.name, changed.name]) {
      if ((await Machine.list(conn)).some((machine) => machine.name === name)) {
        await (await Machine.connect(name, conn)).delete()
      }
    }
    await rm(root, { recursive: true, force: true })
  }
}, 180_000)

// OpenCode uses "/" as the worktree sentinel outside Git repositories. A plugin
// that mounts "/" rejects every ordinary directory before the VM can start.
live("OpenCode bash uses its directory when no Git worktree exists", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "opencode-smol-no-git-"))
  const hooks = await plugin.server(
    { worktree: "/", directory: root } as Parameters<typeof plugin.server>[0],
    { image: null, network: false, onExit: "delete" },
  )
  try {
    const permissions: string[] = []
    const result = await hooks.tool!.bash.execute(
      { command: "printf vm-backed > marker.txt; cat marker.txt", description: "Write marker in isolated VM" },
      {
        worktree: "/", directory: root, abort: new AbortController().signal,
        sessionID: "live", messageID: "live", agent: "build",
        metadata: () => {},
        ask: async (input) => { permissions.push(input.permission) },
      },
    )
    expect(permissions).toEqual(["bash"])
    expect(typeof result === "string" ? result : result.output).toBe("vm-backed")
    expect(await readFile(path.join(root, "marker.txt"), "utf8")).toBe("vm-backed")
  } finally {
    await hooks.dispose?.()
    await rm(root, { recursive: true, force: true })
  }
}, 180_000)
