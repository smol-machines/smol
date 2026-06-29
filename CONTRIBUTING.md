# Contributing to smol

Thanks for your interest! This repo is the open **`smol` SDK + CLI** (Apache-2.0).
A few things are specific to this project, so please read this first.

## The engine lives in a separate repo

The microVM **engine** (`smolvm`, wrapping `libkrun`) is open source in its own
repository — [smol-machines/smolvm](https://github.com/smol-machines/smolvm),
Apache-2.0. The Rust crates here declare a **path dependency** on it
(`smolvm = { path = ".." }`), so a native build needs the `smolvm` repo checked
out alongside this one — **the native cores and the CLI do not build from a
standalone checkout** of just this repo. Maintainers' CI produces the published
native builds.

Practically, that means **most contributions don't need a native build**:

- **Node SDK** — the TypeScript layer (`sdk/node/*.ts`). `npx tsc --noEmit`
  type-checks without the native addon.
- **Python SDK** — the pure-Python layer (`sdk/python/python/smol/*.py`),
  including the cloud transport. `python -m compileall` + the unit/cloud-mock
  tests run with no engine.
- **Docs, examples, tests, the cloud transport, error handling, types.**

The `SDK CI` workflow runs exactly these engine-free checks on every PR (incl.
forks), so you get real signal without engine access. Changes that require a
native rebuild (the `sdk/*/src/*.rs` cores or `src/` CLI) are validated by
maintainers.

## Workflow

1. Open an issue first for anything non-trivial so we can agree on direction.
2. Fork, branch, and keep PRs focused.
3. Match the surrounding style. For TS, keep `tsc --noEmit` clean; for Python,
   keep the package byte-compilable and the unit tests passing.
4. By contributing, you agree your work is licensed under the repo's Apache-2.0
   license.

## Reporting security issues

Do **not** use public issues — see [SECURITY.md](SECURITY.md).
