import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { writeJson } from "../../zg-retrieval/core/io.mjs";
import { verifyModelCacheLocation } from "../runtime.mjs";

test("restored model bytes must occupy the cache path persisted in the index", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "browsecomp-model-location-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await writeJson(join(root, ".zvec-grep/manifest.json"), {
    embeddingRuntimes: {
      "local/potion-multilingual-128m": { cacheDir: "/builder/model-cache" },
    },
  });
  await verifyModelCacheLocation(
    root,
    "local/potion-multilingual-128m",
    "/builder/model-cache",
  );
  await assert.rejects(
    verifyModelCacheLocation(
      root,
      "local/potion-multilingual-128m",
      "/query/model-cache",
    ),
    /persisted cache path/,
  );
});
