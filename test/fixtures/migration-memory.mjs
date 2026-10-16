import assert from "node:assert/strict";
import { mkdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { migrateWorkspaceIndex } from "../../dist/engine/migrate/index.js";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../../dist/engine/transfer/index.js";

const [sourceHome, parent, expected] = process.argv.slice(2);
let maxHeap = 0;
const report = (stage) => {
  maxHeap = Math.max(maxHeap, process.memoryUsage().heapUsed);
  console.log(
    JSON.stringify({
      stage,
      heap: process.memoryUsage().heapUsed,
      rss: process.memoryUsage.rss(),
    }),
  );
};
const destinationRoot = join(parent, "migrated");
await mkdir(destinationRoot);
const migrated = await migrateWorkspaceIndex({
  sourceHome,
  destinationRoot,
  verifySampleLimit: 0,
  onProgress: report,
});
assert.equal(migrated.entitiesConverted, Number(expected));
const manifest = JSON.parse(
  await readFile(join(migrated.destinationHome, "manifest.json"), "utf8"),
);
assert.deepEqual(manifest.rootPaths[0].include, ["*.md"]);
assert.deepEqual(manifest.rootPaths[0].globs, ["!private/**"]);
const artifactPath = join(parent, "artifact");
await exportWorkspaceIndex({ sourceHome, artifactPath, onProgress: report });
const importedRoot = join(parent, "imported");
await mkdir(importedRoot);
const imported = await importWorkspaceIndex({
  artifactPath,
  destinationRoot: importedRoot,
  verifySampleLimit: 0,
  onProgress: report,
});
assert.equal(imported.entitiesImported, Number(expected));
assert.equal(imported.verification.vectorsPreserved, true);
console.log(
  JSON.stringify({ completed: true, entities: Number(expected), maxHeap }),
);
