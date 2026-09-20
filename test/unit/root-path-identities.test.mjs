import assert from "node:assert/strict";
import fs from "node:fs";
import { link, mkdir, writeFile } from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";
import test from "node:test";
import { validateRootPaths } from "../../dist/engine/pipeline/indexing/root-paths.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("root validation distinguishes large file IDs and still detects hard links", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-root-identities-");
  const file = join(root, "direct.ts");
  const otherFile = join(root, "other.ts");
  const directory = join(root, "child");
  const hardLink = join(root, "direct-link.ts");
  await writeFile(file, "export {};\n");
  await writeFile(otherFile, "export {};\n");
  await mkdir(directory);
  await link(file, hardLink);

  // Distinct 64-bit IDs can round to the same Number on Windows filesystems.
  const fileId = 2n ** 54n;
  const identities = new Map([
    [file, fileId],
    [hardLink, fileId],
    [directory, fileId + 1n],
    [otherFile, fileId + 2n],
  ]);
  const statSync = fs.statSync;
  t.mock.method(fs, "statSync", (path, options) => {
    const info = statSync(path, options);
    const identity = identities.get(path);
    if (info && identity !== undefined) {
      info.ino = typeof info.ino === "bigint" ? identity : Number(identity);
    }
    return info;
  });
  syncBuiltinESMExports();
  t.after(() => {
    t.mock.restoreAll();
    syncBuiltinESMExports();
  });

  assert.equal(validateRootPaths([file, otherFile]).length, 2);
  assert.equal(validateRootPaths([file, directory]).length, 2);
  assert.throws(() => validateRootPaths([file, hardLink]), {
    code: "ZVEC_GREP.ENGINE.SCANNER.OVERLAPPING_ROOT_PATHS",
  });

  identities.set(file, 0n);
  identities.set(otherFile, 0n);
  assert.equal(validateRootPaths([file, otherFile]).length, 2);
});
