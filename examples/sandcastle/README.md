# Sandcastle in a local Smol microVM

Use Sandcastle's public isolated sandbox provider interface to run an agent in a local Smol VM. Sandcastle transfers a Git bundle into the VM and syncs changes back to a host worktree. The host `.git` directory is not bind-mounted into the guest.

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

Build your image with a shell, Git, tar, and the chosen agent CLI; permit egress to the image registry and any API hosts that agent needs. `smolmachines` needs a supported local VM host (macOS Apple Silicon or Linux with KVM). The adapter supports Sandcastle's `run()` and `createSandbox()` paths, including streamed output, stdin, file transfer, and Git sync. Sandcastle's `interactive()` needs a TTY API and is not supported here. Smol's cloud SDK currently does not expose streamed exec, which Sandcastle requires for live output and idle timeouts; this adapter targets local VMs.

Validate the integration without agent credentials:

```bash
npm test
npm run typecheck
npm run test:live # boots a real VM and runs Sandcastle against a temporary Git repo
```

The live test pulls `alpine/git:2.47.2` and needs registry access; it uses a scripted agent to verify the full sync round trip. This sample image is for the test only: for an actual agent run, use an image with that agent's CLI installed.
