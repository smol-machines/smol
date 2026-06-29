## What & why

<!-- Brief description of the change and the motivation. Link any issue. -->

## Checklist
- [ ] Focused change; matches surrounding style
- [ ] Node: `npx tsc --noEmit` is clean (if TS touched)
- [ ] Python: package byte-compiles + unit tests pass (if Python touched)
- [ ] Docs updated if behavior changed
- [ ] No secrets / credentials added

> Note: the microVM **engine** ([smol-machines/smolvm](https://github.com/smol-machines/smolvm))
> is a separate repo this one path-depends on, so the native cores / CLI can't be
> built from a standalone checkout of just this repo — engine-free checks run on
> every PR; native builds are validated by maintainers. See CONTRIBUTING.md.
