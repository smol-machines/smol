# Security Policy

`smol` runs code inside isolated microVM sandboxes, so we take isolation and
supply-chain issues seriously.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately via GitHub's **[Private vulnerability reporting](https://github.com/smol-machines/smol/security/advisories/new)**
(repo → **Security** → **Report a vulnerability**). We aim to acknowledge within
a few business days and will coordinate a fix and disclosure timeline with you.

When reporting, please include:

- affected component (CLI, Node SDK, Python SDK, cloud transport) and version,
- a description and, if possible, a minimal reproduction,
- the impact you believe it has.

## In scope

- **Sandbox escape** — guest code affecting the host beyond its configured
  resources, mounts, or network.
- **Privilege or boundary issues** in the boot helper / local transport.
- **Credential or token handling** in the CLI and cloud transport.
- **Supply chain** — integrity of the published `smolmachines` packages and
  bundled binaries.

## Out of scope

- The `smolvm`/`libkrun` engine internals — these live in the separate
  [smol-machines/smolvm](https://github.com/smol-machines/smolvm) repo (report
  those the same way; we route them to the right place).
- Issues requiring a already-compromised host or a privileged local attacker.

## Supported versions

This is alpha software; only the latest published `smolmachines` release is
supported. Please reproduce against the newest version before reporting.
