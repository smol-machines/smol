/**
 * Real cloud-transport test against a LIVE smolfleet `/v1` server (localhost).
 * Driven by SMOL_CLOUD_URL + SMOL_CLOUD_TOKEN (a real smk_ key).
 *
 *   SMOL_CLOUD_URL=http://127.0.0.1:9099 SMOL_CLOUD_TOKEN=smk_… npx tsx test/cloud-real.ts
 */
import { Machine } from '../index';

async function main(): Promise<void> {
  const baseUrl = process.env.SMOL_CLOUD_URL!;
  const apiKey = process.env.SMOL_CLOUD_TOKEN!;
  console.log(`target=${baseUrl} key=${apiKey.slice(0, 8)}…`);

  let m: Machine;
  try {
    m = await Machine.create(
      { image: 'alpine', resources: { cpus: 1, memoryMb: 512, network: true } },
      { target: 'cloud', baseUrl, apiKey },
    );
    console.log('CREATED', m.name, 'state=', await m.state().catch((e) => `(state err: ${e.message})`));
  } catch (e) {
    console.error('CREATE_ERR:', (e as Error).message);
    process.exit(2);
  }

  try {
    const r = await m.exec(['echo', 'cloud-real-ok']);
    console.log('EXEC stdout=', JSON.stringify(r.stdout), 'exit=', r.exitCode);
    await m.writeFile('/tmp/z', 'realrt');
    const b = await m.readFile('/tmp/z');
    console.log('FILE=', b.toString());
    console.log(r.exitCode === 0 && r.stdout.includes('cloud-real-ok') ? 'CLOUD_REAL_OK' : 'CLOUD_REAL_PARTIAL');
  } catch (e) {
    console.error('OP_ERR:', (e as Error).message);
  } finally {
    try {
      await m.delete();
      console.log('deleted');
    } catch (e) {
      console.error('DELETE_ERR:', (e as Error).message);
    }
  }
}

main().catch((e) => {
  console.error('crashed:', e);
  process.exit(1);
});
