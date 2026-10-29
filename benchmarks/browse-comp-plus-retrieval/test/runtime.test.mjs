import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  objectHash,
  readJson,
  sha256,
  writeJson,
} from "../../zg-retrieval/core/io.mjs";
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

test("the frozen model manifest describes a fresh Rust cache, including generated tokenizer config", async () => {
  const lock = await readJson(new URL("../source.lock.json", import.meta.url));
  assert.equal(lock.model_artifacts.length, 3);
  assert.equal(objectHash(lock.model_artifacts), lock.model_artifacts_sha256);
  const config = lock.model_artifacts.find((row) =>
    row.path.endsWith("/tokenizer_config.json"),
  );
  assert.equal(
    config.sha256,
    sha256('{"tokenizer_class":"PreTrainedTokenizer"}\n'),
  );
  assert.equal(
    lock.model_artifacts_sha256,
    "1d2c04d6b5a9ef342e46e32745357f6c09a594f5b99554d8048db83820f0d123",
  );
});
