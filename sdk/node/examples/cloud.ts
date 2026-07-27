import { Machine } from "smolmachines";

async function main() {
  const machine = await Machine.create(
    { image: "alpine:3.20" },
    { target: "cloud" }, // uses SMOL_CLOUD_TOKEN
  );

  try {
    // create() waits for ready=true; state "started" only means VM launched.
    const result = await machine.exec(["echo", "ready for work"]);
    result.assertSuccess();
    console.log(result.stdout);
  } finally {
    await machine.delete();
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
