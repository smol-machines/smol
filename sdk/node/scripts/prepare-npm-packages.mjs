#!/usr/bin/env node
// Assemble the per-platform npm packages from the release artifacts and point
// the main package at them.
//
// Each platform's runtime (the napi addon, the `smol-vmm` boot helper,
// libkrun/libkrunfw and the guest rootfs) ships in its own package, gated by
// `os`/`cpu`/`libc`, and the main `smolmachines` package lists them all as
// exact-version `optionalDependencies` — so npm installs only the host's
// runtime instead of every platform's. The manifests are generated here, at
// publish time, from the main package.json (the napi-rs `prepublish` pattern):
// nothing extra to version-bump, and the committed package.json/lockfile never
// reference packages before they exist on the registry.
//
// Usage (from sdk/node):
//   node scripts/prepare-npm-packages.mjs <artifacts-dir> [--out npm-packages] [--only <triple>...]
// <artifacts-dir> holds the downloaded `node-native-<target>/` artifacts, each
// with `smol.<triple>.node` and `native/<platform>/`. `--only` assembles a
// subset (local measurement); publishing must assemble all of them.

import { chmodSync, cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

// napi triple → the runtime's `${process.platform}-${process.arch}` dir name
// (see PLATFORM_PACKAGES in assets.ts) plus the npm install constraints and the
// library files that must be present (the names the engine and libkrun open).
const PLATFORMS = {
  'darwin-arm64': { dir: 'darwin-arm64', os: 'darwin', cpu: 'arm64', libs: ['libkrun.dylib', 'libkrunfw.5.dylib'], label: 'macOS Apple Silicon' },
  'linux-x64-gnu': { dir: 'linux-x64', os: 'linux', cpu: 'x64', libc: 'glibc', libs: ['libkrun.so', 'libkrunfw.so.5'], label: 'Linux x64 (glibc)' },
  'linux-arm64-gnu': { dir: 'linux-arm64', os: 'linux', cpu: 'arm64', libc: 'glibc', libs: ['libkrun.so', 'libkrunfw.so.5'], label: 'Linux arm64 (glibc)' },
};

function parseArgs(argv) {
  const args = { artifacts: undefined, out: 'npm-packages', only: [] };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--out') args.out = argv[++i];
    else if (arg === '--only') args.only.push(argv[++i]);
    else if (!args.artifacts) args.artifacts = arg;
    else throw new Error(`unexpected argument: ${arg}`);
  }
  if (!args.artifacts) throw new Error('usage: prepare-npm-packages.mjs <artifacts-dir> [--out dir] [--only triple]');
  for (const t of args.only) if (!PLATFORMS[t]) throw new Error(`unknown triple: ${t}`);
  return args;
}

/** First path under `root` (recursively) whose basename is `name`. */
function findFile(root, name) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) {
      const hit = findFile(path, name);
      if (hit) return hit;
    } else if (entry.name === name) {
      return path;
    }
  }
  return undefined;
}

/** First directory under `root` whose path ends with `native/<dir>`. */
function findNativeDir(root, dir) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const path = join(root, entry.name);
    if (entry.name === dir && path.endsWith(join('native', dir))) return path;
    const hit = findNativeDir(path, dir);
    if (hit) return hit;
  }
  return undefined;
}

function fail(message) {
  console.error(`prepare-npm-packages: ${message}`);
  process.exit(1);
}

const args = parseArgs(process.argv.slice(2));
const mainPath = resolve('package.json');
const main = JSON.parse(readFileSync(mainPath, 'utf8'));
const triples = args.only.length ? args.only : Object.keys(PLATFORMS);
const artifacts = resolve(args.artifacts);
const out = resolve(args.out);
rmSync(out, { recursive: true, force: true });

for (const triple of triples) {
  const platform = PLATFORMS[triple];
  const name = `${main.name}-${triple}`;
  const addon = `smol.${triple}.node`;
  const addonSrc = findFile(artifacts, addon);
  const nativeSrc = findNativeDir(artifacts, platform.dir);
  if (!addonSrc) fail(`missing ${addon} under ${artifacts}`);
  if (!nativeSrc) fail(`missing native/${platform.dir}/ under ${artifacts}`);

  const pkgDir = join(out, triple);
  mkdirSync(pkgDir, { recursive: true });
  cpSync(addonSrc, join(pkgDir, addon));
  cpSync(nativeSrc, join(pkgDir, 'native'), { recursive: true });

  // actions/upload-artifact strips the executable bit; npm preserves modes, so
  // restore it on the boot helper or spawning it fails with EACCES.
  const helper = join(pkgDir, 'native', 'smol-vmm');
  if (!existsSync(helper)) fail(`${name}: missing native/smol-vmm`);
  chmodSync(helper, 0o755);
  const rootfs = join(pkgDir, 'native', 'agent-rootfs.tar');
  if (!existsSync(rootfs) || statSync(rootfs).size === 0) fail(`${name}: missing native/agent-rootfs.tar`);
  for (const lib of platform.libs) {
    if (!existsSync(join(pkgDir, 'native', lib))) fail(`${name}: missing native/${lib}`);
  }

  const manifest = {
    name,
    version: main.version,
    description: `smolmachines local engine for ${platform.label}: native addon, boot helper, hypervisor libraries and guest rootfs. Installed automatically by "${main.name}".`,
    main: addon,
    files: [addon, 'native'],
    os: [platform.os],
    cpu: [platform.cpu],
    ...(platform.libc ? { libc: [platform.libc] } : {}),
    license: main.license,
    repository: main.repository,
    homepage: main.homepage,
    author: main.author,
    engines: main.engines,
  };
  writeFileSync(join(pkgDir, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  writeFileSync(
    join(pkgDir, 'README.md'),
    `# ${name}\n\nThe ${platform.label} local engine for [\`${main.name}\`](https://www.npmjs.com/package/${main.name}). ` +
      `Do not install this directly — \`npm install ${main.name}\` pulls in the package for your platform.\n`,
  );
  console.log(`assembled ${name}@${main.version} -> ${pkgDir}`);
}

// Every platform package, pinned to this exact version, so an install always
// pairs the JS layer with the runtime it was released with.
main.optionalDependencies = Object.fromEntries(
  Object.keys(PLATFORMS).map((triple) => [`${main.name}-${triple}`, main.version]),
);
writeFileSync(mainPath, `${JSON.stringify(main, null, 2)}\n`);
console.log(`${main.name}@${main.version}: optionalDependencies -> ${Object.keys(main.optionalDependencies).join(', ')}`);
