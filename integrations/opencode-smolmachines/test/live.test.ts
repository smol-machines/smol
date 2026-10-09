/** Run with `bun run test:live` on macOS Apple Silicon or a Linux KVM host. */
import { expect, test } from "bun:test"
import { mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import { Machine } from "smolmachines"
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
