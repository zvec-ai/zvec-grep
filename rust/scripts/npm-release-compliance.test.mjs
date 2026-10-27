import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  stageCompliance,
  verifyComplianceDirectory,
  verifyPublicationInputs,
} from "./npm-release-compliance.mjs";

test("candidates can omit compliance inputs, but publication cannot", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-compliance-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await stageCompliance(join(root, "missing"), join(root, "candidate"));
  await assert.rejects(
    verifyPublicationInputs({ platforms: [{ target: "missing" }] }, root),
    /publication disabled/,
  );
});

test("packaging copies complete compliance inputs and rejects an empty license", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-compliance-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "source");
  await mkdir(join(source, "LICENSES"), { recursive: true });
  await writeFile(
    join(source, "LICENSES", "project.txt"),
    "Example license text",
  );
  await writeFile(
    join(source, "SBOM.spdx.json"),
    JSON.stringify({
      spdxVersion: "SPDX-2.3",
      dataLicense: "CC0-1.0",
      SPDXID: "SPDXRef-DOCUMENT",
      documentNamespace: "https://example.com/sbom/0.0.1",
      creationInfo: {
        creators: ["Organization: Example"],
        created: "2026-10-13T00:00:00Z",
      },
      packages: [{ name: "example" }],
    }),
  );
  const destination = join(root, "candidate");
  await stageCompliance(source, destination);
  await verifyComplianceDirectory(destination);
  await assert.rejects(verifyPublicationInputs({ platforms: [{
    target: "source", package: "@zvec/base", components: [{ package: "@zvec/base-cuda" }],
  }] }, root), /incomplete compliance files.*source[\\/]cuda/);
  assert.equal(
    await readFile(join(destination, "LICENSES", "project.txt"), "utf8"),
    "Example license text",
  );
  await writeFile(join(source, "LICENSES", "project.txt"), " ");
  await assert.rejects(
    stageCompliance(source, join(root, "incomplete")),
    /publication disabled/,
  );
});
