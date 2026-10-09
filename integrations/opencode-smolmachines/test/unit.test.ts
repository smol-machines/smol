import { describe, expect, test } from "bun:test"
import {
  Sandbox,
  machineConfig,
  machineName,
  truncate,
  type MachineApi,
  type MachineHandle,
} from "../src/sandbox"

type Call = { command: string[]; opts?: Record<string, unknown> }

/** A machine that records what it was asked and answers like smolvm does. */
function fakeMachine(name: string, opts: { fingerprint?: string; state?: string; shell?: string } = {}) {
  const calls: Call[] = []
  let state = opts.state ?? "running"
  let fingerprint = opts.fingerprint
  const events: string[] = []
  const machine: MachineHandle & { calls: Call[]; events: string[] } = {
    name,
    calls,
    events,
    async exec(command, o) {
      calls.push({ command, opts: o as Record<string, unknown> })
      const script = command.join(" ")
      if (script.startsWith("cat /etc/opencode-smolmachines"))
        return fingerprint ? { exitCode: 0, stdout: fingerprint, stderr: "" } : { exitCode: 1, stdout: "", stderr: "no file" }
      if (script.includes("> /etc/opencode-smolmachines")) {
        fingerprint = command[command.length - 1]
        return { exitCode: 0, stdout: "", stderr: "" }
      }
      if (script.includes("command -v")) return { exitCode: 0, stdout: `${opts.shell ?? "/bin/bash"}\n`, stderr: "" }
      const user = command[2]
      if (user === "sleep-forever") return { exitCode: 124, stdout: "start\n", stderr: "\ncommand timed out after 2000ms" }
      if (user === "fail") return { exitCode: 3, stdout: "out\n", stderr: "boom\n" }
      if (user === "abort") throw new Error("aborted")
      return { exitCode: 0, stdout: `ran: ${user}\n`, stderr: "" }
    },
    async state() { return state },
    async start() { events.push("start"); state = "running" },
    async stop() { events.push("stop"); state = "stopped" },
    async delete() { events.push("delete"); state = "deleted" },
  }
  return machine
}

function fakeApi(existing?: ReturnType<typeof fakeMachine>) {
  const created: { config: Record<string, unknown>; machine: ReturnType<typeof fakeMachine> }[] = []
  const api: MachineApi & { created: typeof created } = {
    created,
    async connect(name) {
      if (!existing || existing.events.includes("delete")) throw Object.assign(new Error("VM not found"), { code: "NOT_FOUND" })
      return existing
    },
    async create(config) {
      const machine = fakeMachine(config.name as string)
      created.push({ config, machine })
      return machine
    },
  }
  return api
}

const ROOT = "/home/dev/my app"

describe("machine identity", () => {
  test("one stable machine per project path", () => {
    expect(machineName(ROOT)).toBe(machineName(ROOT + "/"))
    expect(machineName(ROOT)).toMatch(/^opencode-my-app-[0-9a-f]{8}$/)
    expect(machineName(ROOT)).not.toBe(machineName("/home/dev/other/my app"))
  })

  test("the project is mounted at its own path and an allow list turns networking on", () => {
    const config = machineConfig(ROOT, { network: false, allowHosts: ["registry.npmjs.org"] })
    expect(config.mounts).toEqual([{ source: ROOT, target: ROOT }])
    expect(config.network).toBe(true)
    expect(config.resources).toEqual({ allowHosts: ["registry.npmjs.org"] })
    expect(machineConfig(ROOT, { network: false }).network).toBe(false)
  })
})

describe("running commands", () => {
  test("a first command creates the machine, later ones reuse it", async () => {
    const api = fakeApi()
    const sandbox = new Sandbox(ROOT, {}, api)
    let started = 0
    const first = await sandbox.run({ command: "ls", directory: ROOT, onStart: () => started++ })
    await sandbox.run({ command: "pwd", directory: ROOT, onStart: () => started++ })
    expect(api.created.length).toBe(1)
    expect(started).toBe(1)
    expect(first.output).toBe("ran: ls")
    expect(first.metadata).toMatchObject({ exit: 0, sandbox: "smolmachines" })
    const user = api.created[0].machine.calls.filter((c) => c.command[1] === "-c" && c.command[0] === "/bin/bash")
    expect(user.map((c) => c.command[2])).toEqual(["ls", "pwd"])
    expect(user[0].opts).toMatchObject({ workdir: ROOT, timeout: 120 })
  })

  test("a stopped machine with the same shape is started, not rebuilt", async () => {
    const config = machineConfig(ROOT, {})
    const { configFingerprint } = await import("../src/sandbox")
    const existing = fakeMachine(config.name as string, { fingerprint: configFingerprint(config), state: "stopped" })
    const api = fakeApi(existing)
    await new Sandbox(ROOT, {}, api).run({ command: "ls", directory: ROOT })
    expect(existing.events).toEqual(["start"])
    expect(api.created.length).toBe(0)
  })

  test("a machine built with other options is replaced, so an old network policy never lingers", async () => {
    const existing = fakeMachine(machineName(ROOT), { fingerprint: "stale" })
    const api = fakeApi(existing)
    await new Sandbox(ROOT, { allowHosts: ["pypi.org"] }, api).run({ command: "ls", directory: ROOT })
    expect(existing.events).toEqual(["delete"])
    expect(api.created.length).toBe(1)
    expect(api.created[0].config.resources).toEqual({ allowHosts: ["pypi.org"] })
  })

  test("a working directory outside the project is refused before anything runs", async () => {
    const api = fakeApi()
    const sandbox = new Sandbox(ROOT, {}, api)
    await expect(sandbox.run({ command: "ls", directory: ROOT, workdir: "../../etc" })).rejects.toThrow(/outside the sandbox/)
    expect(api.created.length).toBe(0)
    expect(sandbox.resolveWorkdir(ROOT, "src")).toBe(ROOT + "/src")
  })

  test("only named host variables are forwarded", async () => {
    const api = fakeApi()
    const sandbox = new Sandbox(ROOT, { passEnv: ["CI"], env: { MODE: "test" } }, api, { CI: "1", AWS_SECRET_ACCESS_KEY: "nope" })
    await sandbox.run({ command: "env", directory: ROOT })
    const call = api.created[0].machine.calls.find((c) => c.command[2] === "env")
    expect(call?.opts?.env).toEqual({ CI: "1", MODE: "test" })
  })

  test("a failure reports its exit code, a timeout says so", async () => {
    const api = fakeApi()
    const sandbox = new Sandbox(ROOT, {}, api)
    const failed = await sandbox.run({ command: "fail", directory: ROOT })
    expect(failed.output).toBe("out\nboom\n\nExit code: 3")
    const slow = await sandbox.run({ command: "sleep-forever", directory: ROOT, timeoutMs: 2000 })
    expect(slow.metadata.timedOut).toBe(true)
    expect(slow.output).toContain("exceeding its 2000 ms timeout")
    expect(slow.output).not.toContain("command timed out after")
  })

  test("an aborted command reports the abort instead of an error", async () => {
    const api = fakeApi()
    const sandbox = new Sandbox(ROOT, {}, api)
    const controller = new AbortController()
    controller.abort()
    const result = await sandbox.run({ command: "abort", directory: ROOT, signal: controller.signal })
    expect(result.output).toBe("Command aborted.")
  })

  test("an image without bash falls back to sh", async () => {
    const api = fakeApi()
    api.create = async (config) => {
      const machine = fakeMachine(config.name as string, { shell: "/bin/sh" })
      api.created.push({ config, machine })
      return machine
    }
    await new Sandbox(ROOT, {}, api).run({ command: "ls", directory: ROOT })
    expect(api.created[0].machine.calls.some((c) => c.command[0] === "/bin/sh" && c.command[2] === "ls")).toBe(true)
  })
})

describe("shutdown", () => {
  test("stop by default, delete or keep on request, nothing if never used", async () => {
    for (const [onExit, expected] of [[undefined, ["stop"]], ["delete", ["delete"]], ["keep", []]] as const) {
      const api = fakeApi()
      const sandbox = new Sandbox(ROOT, { onExit }, api)
      await sandbox.run({ command: "ls", directory: ROOT })
      await sandbox.dispose()
      expect(api.created[0].machine.events).toEqual([...expected])
    }
    const unused = fakeApi()
    await new Sandbox(ROOT, {}, unused).dispose()
    expect(unused.created.length).toBe(0)
  })
})

test("long output keeps its head and tail", () => {
  const text = "a".repeat(20_000) + "MIDDLE" + "b".repeat(20_000)
  const cut = truncate(text, 1_000)
  expect(cut.startsWith("a".repeat(500))).toBe(true)
  expect(cut.endsWith("b".repeat(500))).toBe(true)
  expect(cut).toContain("characters truncated")
  expect(cut).not.toContain("MIDDLE")
})
