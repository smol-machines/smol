/** Real local-service fork smoke test.
 *
 * Verifies that an image's default workload starts automatically, local ports
 * are discoverable, auto-remapped clone ports become ready, and clone execs use
 * the golden's inherited container overlay while preserving sibling isolation.
 */

import { createServer } from "node:net";
import { Machine } from "../index";

async function availablePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("failed to allocate a local port"));
        return;
      }
      const port = address.port;
      server.close((error) => (error ? reject(error) : resolve(port)));
    });
  });
}

async function page(machine: Machine): Promise<string> {
  const response = await machine.fetch(80);
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return await response.text();
}

async function main(): Promise<void> {
  const suffix = `${process.pid}-${Date.now()}`;
  const golden = await Machine.create({
    name: `sdk-golden-${suffix}`,
    image: "nginx:1.27-alpine",
    ports: [{ host: await availablePort(), guest: 80 }],
    resources: { cpus: 2, memoryMb: 1024, network: true },
    persistent: true,
    forkable: true,
  });
  const clones: Machine[] = [];

  try {
    if (!(await page(golden)).includes("Welcome to nginx")) {
      throw new Error("golden image workload did not serve its default page");
    }
    const cloneOne = await golden.fork(`sdk-clone-one-${suffix}`);
    clones.push(cloneOne);
    const cloneTwo = await golden.fork(`sdk-clone-two-${suffix}`);
    clones.push(cloneTwo);

    if (cloneOne.endpoint(80).httpUrl === cloneTwo.endpoint(80).httpUrl) {
      throw new Error("auto-remapped clones received the same host port");
    }
    await cloneOne.writeFile(
      "/usr/share/nginx/html/index.html",
      Buffer.from("clone-one"),
    );
    if (
      (await cloneOne.readFile("/usr/share/nginx/html/index.html")).toString() !==
      "clone-one"
    ) {
      throw new Error("clone file APIs did not use the inherited image overlay");
    }

    if ((await page(cloneOne)).trim() !== "clone-one") {
      throw new Error("clone file write did not mutate its inherited image overlay");
    }
    if (!(await page(cloneTwo)).includes("Welcome to nginx")) {
      throw new Error("clone overlay mutation leaked into a sibling");
    }
    await cloneTwo.stop();
    await cloneTwo.start();
    if (!(await page(cloneTwo)).includes("Welcome to nginx")) {
      throw new Error("image workload did not relaunch after clone restart");
    }
    console.log("fork-service-e2e: passed");
  } finally {
    await Promise.allSettled(clones.map((clone) => clone.delete()));
    await golden.delete();
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
