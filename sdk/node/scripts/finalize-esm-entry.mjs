// Publish the ESM build of the dual-format `smolmachines/openai-agents` entry
// beside its CommonJS build. It imports the rest of the SDK from the CJS files
// next to it (`./machine.js`), so only the entry itself is kept.
import { renameSync, rmSync } from 'node:fs';

renameSync('dist/esm/openai-agents.js', 'dist/openai-agents.mjs');
rmSync('dist/esm', { recursive: true, force: true });
