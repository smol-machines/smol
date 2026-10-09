# opencode-smolmachines

Run [OpenCode](https://opencode.ai)'s shell commands inside a [smol machine](https://smolmachines.com): a hardware-isolated Linux microVM with its own kernel, on macOS and Linux.

The agent keeps working exactly as before. What changes is where its commands run:

- **Isolated from your machine.** Commands see only the project directory. Your home directory, SSH keys, cloud credentials and shell environment are not visible.
- **Egress you control.** Allow only the hosts a project needs, such as your package registries, and nothing else is reachable.
- **Fast after the first run.** Each project configuration gets a machine that is stopped when OpenCode exits and started again next time, with everything installed in it still there.

File reads and edits still go through OpenCode's own tools on the host, against the same mounted directory, under OpenCode's usual permissions.

## Install

You need smol machines' runtime prerequisites: KVM on Linux, or Apple Silicon on macOS.

Until the npm package is published, build the plugin from this repository:

```bash
cd integrations/opencode-smolmachines
bun install --frozen-lockfile
bun run build
```

Point your project's `opencode.json` at the resulting file (replace the absolute path with your checkout):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["file:///absolute/path/to/smol/integrations/opencode-smolmachines/dist/index.js"]
}
```

The built plugin resolves its dependencies from the checkout's `node_modules`, so keep the checkout in place. Once `opencode-smolmachines` is published to npm, use `"plugin": ["opencode-smolmachines"]` instead. The first command with the default image pulls it and boots the VM; later sessions start the existing machine.

## Options

Pass options as the second element of the plugin entry. If you installed from source, use the file URL above in place of the npm package name:

```json
{
  "plugin": [
    [
      "opencode-smolmachines",
      {
        "image": "node:22-bookworm",
        "allowHosts": ["registry.npmjs.org", "github.com", "codeload.github.com"],
        "passEnv": ["CI"],
        "cpus": 4,
        "memoryMb": 4096
      }
    ]
  ]
}
```

| Option | Default | What it does |
|---|---|---|
| `image` | `node:22-bookworm` | Any OCI image. The default has git, curl, Python 3, a C toolchain and Node; `null` uses the built-in shell-only guest without a registry pull. |
| `allowHosts` | none | Only these hosts are reachable from the machine. |
| `allowCidrs` | none | Only these IP ranges are reachable. |
| `network` | `true` | Set `false` for no network at all. Ignored when an allow list is set. |
| `passEnv` | `[]` | Host environment variables to forward. Nothing is forwarded by default. |
| `env` | `{}` | Variables to set for every command. |
| `cpus`, `memoryMb` | runtime default | Machine size. |
| `mounts` | `[]` | Extra host directories: `{ "source", "target", "readOnly" }`. |
| `shell` | `bash` | Shell inside the machine; falls back to `sh` if the image has no bash. |
| `onExit` | `stop` | `stop` keeps the machine for next time, `delete` removes it, `keep` leaves it running. |

Changing the image, mounts, size or network policy creates a separate machine before the next command, so it cannot boot under the old policy. The previous machine and its installed packages remain available if you switch back; remove unused machines with `smolvm machine delete`.

## How it works

The plugin registers a tool named `bash`, which replaces OpenCode's built-in shell tool, so prompts, agents and saved permissions keep working. Each command runs through the [smolmachines](https://www.npmjs.com/package/smolmachines) SDK in a machine named `opencode-<project>-<path-hash>-<config-hash>`, with the project mounted at the same absolute path it has on the host. Permission prompts are kept: the tool asks OpenCode for `bash` permission exactly as the built-in tool does.

To remove a project's machine, list its name with `smolvm machine ls` and run `smolvm machine delete --name <name>`, or set `"onExit": "delete"`.

## License

Apache-2.0
