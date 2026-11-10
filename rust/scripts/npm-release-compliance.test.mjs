import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { stageCompliance } from "./npm-release-compliance.mjs";

test("packaging accepts a missing supplemental compliance directory", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-compliance-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await stageCompliance(join(root, "missing"), join(root, "candidate"));
  assert.deepEqual(await readdir(root), []);
});

for (const names of [[], ["LICENSES"], ["SBOM.spdx.json"], ["LICENSES", "SBOM.spdx.json"]]) {
  test(`packaging preserves optional inputs: ${names.join(", ") || "none"}`, async (t) => {
    const root = await mkdtemp(join(tmpdir(), "zg-compliance-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const source = join(root, "source");
    const destination = join(root, "candidate");
    await mkdir(source);
    await mkdir(destination);
    const inputs = {
      LICENSES: { path: join("LICENSES", "dependency", "LICENSE"), text: "Example license text\n" },
      "SBOM.spdx.json": { path: "SBOM.spdx.json", text: '{"spdxVersion":"SPDX-2.3"}\n' },
    };
    for (const name of names) {
      const input = inputs[name];
      await mkdir(join(source, input.path, ".."), { recursive: true });
      await writeFile(join(source, input.path), input.text);
    }
    await stageCompliance(source, destination);
    assert.deepEqual((await readdir(destination)).sort(), [...names].sort());
    for (const name of names) {
      const input = inputs[name];
      assert.equal(await readFile(join(destination, input.path), "utf8"), input.text);
    }
  });
}
