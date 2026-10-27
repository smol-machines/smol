/**
 * The OpenAI Agents SDK sandbox client on real local machines: the SDK's own
 * runner drives a scripted model whose tool calls run in a microVM, then the
 * session API is exercised directly. Needs KVM (or Apple Silicon) and
 * outbound HTTPS.
 */
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { applyDiff, run, setTracingDisabled, Usage } from '@openai/agents-core';
import { Manifest, SandboxAgent, shell } from '@openai/agents-core/sandbox';
import { assistantMessage, functionCall, modelResponse, ScriptedModel } from '@openai/agents-core/testing';
import { Machine } from '../index';
import { SmolmachinesSandboxClient, type SmolmachinesSandboxSession } from '../openai-agents';

const image = process.env.SMOL_TEST_IMAGE ?? 'alpine';
let failures = 0;
function check(label: string, ok: boolean, detail = '') {
  if (!ok) failures++;
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${label}${detail ? `  (${detail})` : ''}`);
}
async function rejects(p: PromiseLike<unknown>): Promise<string> {
  try {
    await p;
    return '';
  } catch (e) {
    return `${(e as Error).name}: ${(e as Error).message}`;
  }
}
const exists = async (name: string) => (await Machine.connect(name, { target: 'local', handleSignals: false }).then(() => true, () => false));
// A 1x1 PNG.
const PNG = Uint8Array.from(Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGBgAAAABQABh6FO1AAAAABJRU5ErkJggg==', 'base64'));

async function main() {
  setTracingDisabled(true);
  const base = mkdtempSync(join(tmpdir(), 'oai-smol-'));
  mkdirSync(join(base, 'project/lib'), { recursive: true });
  writeFileSync(join(base, 'project/lib/util.py'), 'print("from the host")\n');
  const outside = mkdtempSync(join(tmpdir(), 'oai-outside-'));
  writeFileSync(join(outside, 'secret.txt'), 'host secret\n');
  symlinkSync(join(outside, 'secret.txt'), join(base, 'link.txt'));

  const manifest = new Manifest({
    entries: {
      'README.md': { type: 'file', content: '# Demo\nA tiny project.\n' },
      src: { type: 'dir', children: { 'app.py': { type: 'file', content: 'print("hi")\n' } } },
      project: { type: 'local_dir', src: 'project' },
    },
  });
  const client = new SmolmachinesSandboxClient({ image, localSourceBaseDir: base, commandTimeoutMs: 3_000 });

  // 1. The SDK runner, with a scripted model calling exec_command.
  const model = new ScriptedModel([
    modelResponse({
      output: [functionCall('exec_command', { cmd: 'cat README.md src/app.py project/lib/util.py; uname -s' }, { callId: 'c1' })],
      usage: new Usage(),
    }),
    modelResponse({ output: [assistantMessage('done')], usage: new Usage() }),
  ]);
  const agent = new SandboxAgent({ name: 'Coder', model, defaultManifest: manifest, capabilities: [shell()] });
  let t0 = Date.now();
  const result = await run(agent, 'Look around.', { sandbox: { client } });
  const toolOutput = JSON.stringify(result.newItems.filter((item) => item.type === 'tool_call_output_item').map((item) => item.output));
  check('the runner runs a tool call in the machine', /A tiny project/.test(toolOutput) && /print\(\\"hi\\"\)/.test(toolOutput), `${Date.now() - t0} ms`);
  check('local_dir sources are copied in', /from the host/.test(toolOutput));
  check('commands run on a Linux kernel', /Linux/.test(toolOutput));
  check('the runner finishes with the model reply', result.finalOutput === 'done', String(result.finalOutput));
  const leftovers = await Machine.list({ target: 'local', handleSignals: false }, { labels: { 'openai-agents-sandbox': 'true' } });
  check('an owned session is deleted after the run', !leftovers.some((m) => m.name?.startsWith('openai-sbx-')), leftovers.map((m) => m.name).join(','));

  // 2. The session API directly.
  t0 = Date.now();
  const session = (await client.create(manifest)) as SmolmachinesSandboxSession;
  console.log(`  (session created in ${Date.now() - t0} ms)`);
  try {
    const out = await session.execCommand({ cmd: 'pwd; echo err >&2; exit 3' });
    check('exec reports cwd, output and exit code', /\/workspace/.test(out) && /err/.test(out) && /exited with code 3/.test(out));
    const sub = await session.execCommand({ cmd: 'pwd', workdir: 'src' });
    check('a relative workdir resolves under the workspace', /\/workspace\/src/.test(sub));
    check('a workdir outside the workspace is refused', /UserError|must stay|escape|outside/i.test(await rejects(session.execCommand({ cmd: 'true', workdir: '../../etc' }))));
    const timeout = await rejects(session.execCommand({ cmd: 'sleep 30 & sleep 31' }));
    check('a command past its timeout fails as a timeout', /SandboxExecTimeoutError|timed out/.test(timeout), timeout.slice(0, 80));
    const left = await session.execCommand({ cmd: "pgrep -f 'sleep 3[01]' || echo none" });
    check('and nothing it started is left running', /none/.test(left), left.slice(-40));
    check('tty is refused, not ignored', /unsupported|tty/i.test(await rejects(session.execCommand({ cmd: 'true', tty: true }))));

    const editor = session.createEditor();
    await editor.createFile({ type: 'create_file', path: 'notes/todo.md', diff: '+one\n+two\n' });
    await editor.updateFile({ type: 'update_file', path: 'notes/todo.md', diff: '@@\n one\n-two\n+three\n' });
    const edited = new TextDecoder().decode(await session.readFile({ path: 'notes/todo.md' }));
    const expected = applyDiff(applyDiff('', '+one\n+two\n', 'create'), '@@\n one\n-two\n+three\n');
    check('apply_patch creates and updates files as the SDK diff does', edited === expected, JSON.stringify(edited));
    await editor.updateFile({ type: 'update_file', path: 'notes/todo.md', moveTo: 'notes/done.md', diff: '@@\n one\n' });
    check('apply_patch moves files', (await session.pathExists('notes/done.md')) && !(await session.pathExists('notes/todo.md')));
    await editor.deleteFile({ type: 'delete_file', path: 'notes/done.md' });
    check('apply_patch deletes files', !(await session.pathExists('notes/done.md')));
    check('creating an existing file is refused', /already exists/.test(await rejects(editor.createFile({ type: 'create_file', path: 'README.md', diff: '+x\n' }))));

    const listing = await session.listDir({ path: '.' });
    const kinds = Object.fromEntries(listing.map((entry) => [entry.name, entry.type]));
    check('listDir reports files and directories', kinds['README.md'] === 'file' && kinds.src === 'dir' && kinds.project === 'dir', JSON.stringify(kinds));
    check('directoryExists and pathExists agree with the machine', (await session.directoryExists('src')) && !(await session.directoryExists('README.md')) && !(await session.pathExists('nope')));
    check('reading a missing file is a not-found error', /NotFound|not found/i.test(await rejects(session.readFile({ path: 'nope.txt' }))));
    check('reading outside the workspace is refused', /UserError|outside|escape|must/i.test(await rejects(session.readFile({ path: '../etc/passwd' }))));
    check('maxBytes truncates a read', (await session.readFile({ path: 'README.md', maxBytes: 4 })).byteLength === 4);

    await session.materializeEntry({ path: 'img/dot.png', entry: { type: 'file', content: PNG } });
    const shown = await session.viewImage({ path: 'img/dot.png' });
    check('view_image returns the image', shown.type === 'image' && (shown.image as { mediaType?: string }).mediaType === 'image/png');
    check('materializeEntry records the entry in the manifest', 'img/dot.png' in session.state.manifest.entries);

    check('a local source outside the base directory is refused', /must stay within/.test(await rejects(session.materializeEntry({ path: 'x', entry: { type: 'local_file', src: join(outside, 'secret.txt') } }))));
    check('a local source that is a symlink is refused', /symbolic links/.test(await rejects(session.materializeEntry({ path: 'y', entry: { type: 'local_file', src: 'link.txt' } }))));
    check('git_repo entries are refused, not ignored', /git_repo/.test(await rejects(session.materializeEntry({ path: 'z', entry: { type: 'git_repo', repo: 'a/b' } }))));
  } finally {
    await session.delete();
  }
  check('deleting a session deletes its machine', !(await exists(session.machineName)));

  // 3. Egress limited to an allow list.
  const limited = (await new SmolmachinesSandboxClient({ image, allowHosts: ['example.com'] }).create(new Manifest())) as SmolmachinesSandboxSession;
  try {
    const net = await limited.execCommand({
      cmd: 'wget -q -T 8 -O /dev/null https://example.com && echo allowed; wget -q -T 8 -O /dev/null https://example.org && echo leaked || echo blocked',
    });
    check('the allow list admits its host and blocks others', /allowed/.test(net) && /blocked/.test(net) && !/leaked/.test(net));
  } finally {
    await limited.delete();
  }

  // 4. Preserve, serialize and resume.
  const keeper = new SmolmachinesSandboxClient({ image, preserveOnExit: true });
  const kept = (await keeper.create(new Manifest({ entries: { 'state.txt': { type: 'file', content: 'before\n' } } }))) as SmolmachinesSandboxSession;
  await kept.execCommand({ cmd: 'echo after >> state.txt' });
  check('a preserving client can persist owned sessions', keeper.canPersistOwnedSessionState());
  const saved = JSON.parse(JSON.stringify(await keeper.serializeSessionState(kept.state)));
  await kept.delete({ reason: 'cleanup', preserveOwnedSessions: true });
  check('a preserved session keeps its machine', await exists(kept.machineName));
  const resumed = await keeper.resume(await keeper.deserializeSessionState(saved));
  const state = await resumed.execCommand({ cmd: 'cat state.txt' });
  check('a resumed session has its files', /before\nafter/.test(state), state.slice(-30));
  check('bad serialized state is refused', /names no machine/.test(await rejects(keeper.deserializeSessionState({ machine: 'someone-elses-vm' }))));
  await resumed.delete();
  check('and is deleted when done', !(await exists(kept.machineName)));

  rmSync(base, { recursive: true, force: true });
  rmSync(outside, { recursive: true, force: true });
  console.log(`\n${failures === 0 ? 'ALL PASSED' : `${failures} FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error('OPENAI AGENTS E2E CRASHED:', e);
  process.exit(2);
});
