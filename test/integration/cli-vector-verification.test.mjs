import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdir } from "node:fs/promises";
import { join, resolve } from "node:path";
import test from "node:test";
import { migrateWorkspaceIndex } from "../../dist/engine/migrate/index.js";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../../dist/engine/transfer/index.js";
import { largeLegacy } from "../helpers/large-legacy.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
useIsolatedZvecGrepHome();
const exec = promisify(execFile),
  cli = resolve("dist/cli/index.js");
for (const operation of ["migrate", "import"])
  for (const count of [128, 300]) {
    test(`F7 CLI ${operation} reports preserved non-exact vectors and ${count > 256 ? "sampled" : "all"} comparison count`, async (t) => {
      const parent = await createTemporaryDirectory(t, "zg-cli-vectors-");
      const { sourceHome } = await largeLegacy(parent, count);
      const destinationRoot = join(parent, "oracle"),
        actual = join(parent, "cli");
      await mkdir(destinationRoot);
      await mkdir(actual);
      let oracle, args;
      if (operation === "migrate") {
        oracle = await migrateWorkspaceIndex({
          sourceHome,
          destinationRoot,
          verifySampleLimit: 256,
        });
        args = ["--migrate-index", sourceHome, actual];
      } else {
        const artifactPath = join(parent, "artifact");
        await exportWorkspaceIndex({ sourceHome, artifactPath });
        oracle = await importWorkspaceIndex({
          artifactPath,
          destinationRoot,
          verifySampleLimit: 256,
        });
        args = ["--import-index", artifactPath, actual];
      }
      assert.equal(
        oracle.verification.vectorsExact,
        false,
        "the fixture must exercise permitted non-exact vectors",
      );
      assert.equal(oracle.verification.vectorsPreserved, true);
      assert.equal(oracle.verification.vectorsSampled, count > 256);
      const result = await exec(process.execPath, [cli, ...args], {
        timeout: 60000,
      });
      assert.match(
        result.stdout,
        /vectors preserved \(cosine tolerance: at most two float32 steps\)/,
      );
      assert.ok(
        result.stdout.includes(
          `(${oracle.verification.vectorsCompared} compared, ${count > 256 ? "sampled" : "all"})`,
        ),
        result.stdout,
      );
      assert.doesNotMatch(result.stdout, /vectors exact/);
    });
  }
