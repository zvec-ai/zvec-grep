import assert from "node:assert/strict";
import fs from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";
import test from "node:test";
import { largeLegacy } from "../helpers/large-legacy.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { migrateWorkspaceIndex } from "../../dist/engine/migrate/index.js";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../../dist/engine/transfer/index.js";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
useIsolatedZvecGrepHome();
async function fixture(t, operation) {
  const parent = await createTemporaryDirectory(t, "zg-early-destination-");
  const { sourceHome } = await largeLegacy(parent, 2);
  const destinationRoot = join(parent, "destination"),
    artifactPath = join(parent, "artifact");
  await mkdir(destinationRoot);
  if (operation === "import")
    await exportWorkspaceIndex({ sourceHome, artifactPath });
  const home =
    operation === "export" ? artifactPath : join(destinationRoot, ".zvec-grep");
  return {
    home,
    run: (onProgress) =>
      operation === "migrate"
        ? migrateWorkspaceIndex({ sourceHome, destinationRoot, onProgress })
        : operation === "export"
          ? exportWorkspaceIndex({ sourceHome, artifactPath, onProgress })
          : importWorkspaceIndex({ artifactPath, destinationRoot, onProgress }),
  };
}
for (const operation of ["migrate", "export", "import"])
  test(`F9 ${operation} rejects an occupied destination before source copy or scan`, async (t) => {
    const f = await fixture(t, operation);
    await mkdir(f.home);
    const marker = join(f.home, "manifest.json");
    await writeFile(marker, "preserve existing index");
    let copies = 0,
      reads = 0;
    const original = fs.cpSync;
    fs.cpSync = (...args) => {
      if (String(args[1]).includes("zg-transfer-source-")) copies++;
      return original(...args);
    };
    syncBuiltinESMExports();
    try {
      await assert.rejects(
        f.run((stage) => {
          if (stage === "read") reads++;
        }),
        /already contains/,
      );
    } finally {
      fs.cpSync = original;
      syncBuiltinESMExports();
    }
    assert.equal(
      copies,
      0,
      "occupied destinations must be rejected before native source copying",
    );
    assert.equal(
      reads,
      0,
      "occupied destinations must be rejected before a source scan",
    );
    assert.equal(await readFile(marker, "utf8"), "preserve existing index");
  });
for (const operation of ["migrate", "export", "import"])
  test(`F9 ${operation} repeats destination validation under its final reservation lock`, async (t) => {
    const f = await fixture(t, operation);
    let injected = false;
    await assert.rejects(
      f.run((stage) => {
        if (stage === "read" && !injected) {
          fs.mkdirSync(f.home);
          fs.writeFileSync(join(f.home, "manifest.json"), "competing index");
          injected = true;
        }
      }),
      /already contains/,
    );
    assert.equal(injected, true);
    assert.equal(
      await readFile(join(f.home, "manifest.json"), "utf8"),
      "competing index",
    );
  });

test("F9 export identifies an occupied transfer artifact without changing it", async (t) => {
  const f = await fixture(t, "export");
  await mkdir(f.home);
  const marker = join(f.home, "format.json");
  await writeFile(marker, "preserve existing artifact");
  await assert.rejects(
    f.run(),
    /Destination already contains a transfer artifact/,
  );
  assert.equal(await readFile(marker, "utf8"), "preserve existing artifact");
});
