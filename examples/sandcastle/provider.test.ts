import { strict as assert } from "node:assert";
import { test } from "node:test";
import { smol } from "./provider.js";

const options = {
  image: "alpine/git:2.47.2",
  allowHosts: ["registry-1.docker.io", "auth.docker.io"],
};

test("Smol is an isolated Sandcastle provider with explicit guest egress", () => {
  const provider = smol({ ...options, env: { TEST: "value" } });
  assert.equal(provider.name, "smol");
  assert.deepEqual(provider.env, { TEST: "value" });
  assert.throws(() => smol({ ...options, image: "" }), /image/);
  assert.throws(() => smol({ ...options, allowHosts: [] }), /allowHosts/);
  assert.throws(() => smol({ ...options, maxOutputTailChars: 0 }), /positive integer/);
});
