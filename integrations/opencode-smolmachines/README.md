# opencode-smolmachines

Run [OpenCode](https://opencode.ai)'s shell commands inside a [smol machine](https://smolmachines.com): a hardware-isolated Linux microVM with its own kernel, on macOS and Linux.

The agent keeps working exactly as before. What changes is where its commands run:

- **Isolated from your machine.** Commands see only the project directory. Your home directory, SSH keys, cloud credentials and shell environment are not visible.
- **Egress you control.** Allow only the hosts a project needs, such as your package registries, and nothing else is reachable.
- **Fast after the first run.** Each project gets one machine that is stopped when OpenCode exits and started again next time, with everything installed in it still there.

File reads and edits still go through OpenCode's own tools on the host, against the same mounted directory, under OpenCode's usual permissions.

## Install

You need smol machines' runtime prerequisites: KVM on Linux, or Apple Silicon on macOS.

Add the plugin to your `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["opencode-smolmachines"]
}
```

That's it. The first command in a project pulls the image and boots the machine, about 30 seconds for the default image. Later sessions start the existing machine in a few seconds.

## Options

Pass options as the second element of the plugin entry:

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
| `image` | `node:22-bookworm` | Any OCI image. The default has git, curl, Python 3, a C toolchain and Node. |
| `allowHosts` | none | Only these hosts are reachable from the machine. |
| `allowCidrs` | none | Only these IP ranges are reachable. |
| `network` | `true` | Set `false` for no network at all. Ignored when an allow list is set. |
| `passEnv` | `[]` | Host environment variables to forward. Nothing is forwarded by default. |
| `env` | `{}` | Variables to set for every command. |
| `cpus`, `memoryMb` | runtime default | Machine size. |
| `mounts` | `[]` | Extra host directories: `{ "source", "target", "readOnly" }`. |
| `shell` | `bash` | Shell inside the machine; falls back to `sh` if the image has no bash. |
| `onExit` | `stop` | `stop` keeps the machine for next time, `delete` removes it, `keep` leaves it running. |

Changing the image, mounts, size or network policy replaces the project's machine on the next command, so an old policy never lingers.

## How it works

The plugin registers a tool named `bash`, which replaces OpenCode's built-in shell tool, so prompts, agents and saved permissions keep working. Each command runs through the [smolmachines](https://www.npmjs.com/package/smolmachines) SDK in a machine named `opencode-<project>-<hash>`, with the project mounted at the same absolute path it has on the host. Permission prompts are kept: the tool asks OpenCode for `bash` permission exactly as the built-in tool does.

To remove a project's machine, run `smolvm machine delete --name opencode-<project>-<hash>`, or set `"onExit": "delete"`.

## License

Apache-2.0
