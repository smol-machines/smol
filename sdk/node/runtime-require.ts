/** Load the native half of the SDK through Node's own module loader.
 *
 *  Bundlers (webpack, Turbopack, esbuild, Vite SSR) follow every `require(...)`
 *  and `require.resolve(...)` they can see. Following `./binding.js` leads them
 *  into the platform `.node` addon, which they then try to parse as JavaScript
 *  ("Module parse failed: Unexpected character"), so `import 'smolmachines'` in
 *  a Next.js route failed to build unless the app listed the package in
 *  `serverExternalPackages`. Bundling also moves `__dirname` to the bundler's
 *  output directory, so the bundled boot helper and libraries could no longer
 *  be found next to this file.
 *
 *  Everything here reaches Node's `module` builtin without a call a bundler can
 *  trace, and locates the installed package on disk, so the native addon and
 *  its assets load from `node_modules` whether or not the SDK's JavaScript was
 *  bundled. The pure-JavaScript cloud path never touches this file.
 */

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

type ModuleApi = { createRequire(filename: string): NodeRequire };

function moduleApi(): ModuleApi {
  const getBuiltinModule = (process as { getBuiltinModule?: (id: string) => unknown })
    .getBuiltinModule;
  if (typeof getBuiltinModule === 'function') {
    return getBuiltinModule('node:module') as ModuleApi;
  }
  // Node older than 20.16 / 22.3 has no getBuiltinModule. A direct eval keeps
  // the CommonJS `require` in scope while hiding the call from static analysis.
  // eslint-disable-next-line no-eval
  return eval('require')('node:module') as ModuleApi;
}

/** A `require` that resolves relative to `dir`, invisible to bundlers. */
export function requireFrom(dir: string): NodeRequire {
  return moduleApi().createRequire(join(dir, 'noop.js'));
}

function isSdkPackage(dir: string): boolean {
  const manifest = join(dir, 'package.json');
  if (!existsSync(manifest)) return false;
  try {
    return (JSON.parse(readFileSync(manifest, 'utf8')) as { name?: string }).name === 'smolmachines';
  } catch {
    return false;
  }
}

let cachedSdkDir: string | null | undefined;

/** The directory holding `binding.js` and the source-layout `native/` assets:
 *  this file's own directory when the SDK runs unbundled (the package root from
 *  source, `dist/` when built), otherwise the installed package's `dist/`,
 *  resolved from the app's working directory. Undefined when neither exists. */
export function sdkDir(): string | undefined {
  if (cachedSdkDir !== undefined) return cachedSdkDir ?? undefined;
  let found: string | undefined;
  if (existsSync(join(__dirname, 'binding.js')) && [__dirname, join(__dirname, '..')].some(isSdkPackage)) {
    found = __dirname;
  } else {
    try {
      // The package's `exports` map only exposes ".", so resolve its entry
      // (`dist/index.js`) rather than `smolmachines/package.json`.
      const dist = dirname(requireFrom(process.cwd()).resolve('smolmachines'));
      if (existsSync(join(dist, 'binding.js'))) found = dist;
    } catch {
      found = undefined;
    }
  }
  cachedSdkDir = found ?? null;
  return found;
}
