# Sandcastle in a Smol microVM

Use Sandcastle's public isolated sandbox provider interface to run an agent in a local or cloud Smol VM. Sandcastle transfers a Git bundle into the VM and syncs changes back to a host worktree. The host `.git` directory is not bind-mounted into the guest.

```bash
cd examples/sandcastle
npm ci
```

Import `smol` from this example's `provider.ts` (copy it into your project if you prefer):

```ts
import { run, claudeCode } from "@ai-hero/sandcastle";
import { smol } from "./provider.js";

await run({
  agent: claudeCode("claude-opus-4-8"),
  sandbox: smol({
    image: "registry.example.com/agent-with-git:latest",
    allowHosts: ["registry.example.com", "api.anthropic.com"],
  }),
  prompt: "Update the tests",
});
```

Build your image with a shell, Git, tar, and the chosen agent CLI. Allow egress to the image registry and the agent's API hosts. The default `target: "local"` needs a supported host (macOS Apple Silicon or Linux with KVM).

For Smol Cloud, add `target: "cloud"` to the `smol()` options and authenticate with `SMOL_CLOUD_TOKEN`, a Smol CLI login, or `cloud: { apiKey: "..." }`. The cloud target uses the same streamed exec and file APIs. `npm test` checks the cloud transport against an HTTP mock.

The adapter supports Sandcastle's `run()` and `createSandbox()` paths, including streamed output, stdin, file transfer, and Git sync. Git bundles and upload chunks use the VM's persistent `/workspace` disk rather than the RAM-backed `/tmp`; allow enough guest disk for the bundle and cloned repository. Sandcastle's `interactive()` needs a TTY API and is not supported here.

Validate the integration without agent credentials:

```bash
npm test
npm run typecheck
npm run test:live # boots a real local VM and runs Sandcastle against a temporary Git repo
npm run test:cloud-transport:live # drives the published cloud HTTP transport through a local VM
```

The live test pulls `alpine/git:2.47.2` and needs registry access; it uses a scripted agent to verify the full sync round trip. This sample image is for the test only: for an actual agent run, use an image with that agent's CLI installed.
