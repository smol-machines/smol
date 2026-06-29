import { Machine } from 'smolmachines';

async function main() {
  const m = await Machine.create({
    resources: { cpus: 2, memoryMb: 1024, network: true },
  });

  try {
    const res = await m.run('python:3.12', [
      'python',
      '-c',
      'import sys; print(sys.version)',
    ]);
    res.assertSuccess();
    console.log(res.stdout);

    await m.writeFile('/tmp/note.txt', 'hello from the host');
    const back = await m.readFile('/tmp/note.txt');
    console.log('readback:', back.toString());
  } finally {
    await m.delete();
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
