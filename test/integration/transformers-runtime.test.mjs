import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const childPath = fileURLToPath(
  new URL("../helpers/transformers-runtime-child.mjs", import.meta.url),
);

async function runScenario(scenario) {
  const { stderr } = await execFileAsync(
    process.execPath,
    [childPath, scenario],
    {
      timeout: 30_000,
      maxBuffer: 1024 * 1024,
    },
  );
  assert.doesNotMatch(stderr, /falling back to CPU/);
}

for (const device of ["default", "auto", "cpu"]) {
  test(`real Transformers.js embeds offline with device=${device}`, async () => {
    await runScenario(device);
  });
}

test("real Transformers.js initialization failure is terminal until a fresh process", async () => {
  await runScenario("failure");
  await runScenario("cpu");
});

test("a real tokenizer initialization failure does not disable other models", async () => {
  await runScenario("tokenizer-failure");
});
