import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  Collection,
  CollectionRegistry,
  isCollectionIndexed,
} from "../../dist/engine/collection/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

test("collection registry covers lifecycle, caching, rename, roots, disable, read-only, and schema errors", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-collection-registry-",
  );
  const home = join(temporaryDirectory, "home");
  const root = join(temporaryDirectory, "repo");
  const secondRoot = join(temporaryDirectory, "repo-two");
  await mkdir(root, { recursive: true });
  await mkdir(secondRoot, { recursive: true });
  await writeFile(join(root, "alpha.ts"), "export const Alpha = 1;\n");
  await writeFile(join(secondRoot, "beta.ts"), "export const Beta = 2;\n");

  const withoutModelHome = join(temporaryDirectory, "without-model");
  const withoutModel = new CollectionRegistry(withoutModelHome);
  assert.throws(
    () => withoutModel.create("missing-model", [root]),
    /requires an embedding model/,
  );
  assert.throws(
    () => withoutModel.prepareIndex("missing-model", [root]),
    /requires an embedding model/,
  );
  withoutModel.close();

  const embedding = new FakeEmbeddingModel();
  const registry = new CollectionRegistry(home, embedding);
  t.after(() => registry.close());
  const created = registry.create("docs", [root]);
  assert.equal(isCollectionIndexed(created), true);
  assert.equal(registry.has("docs"), true);
  assert.equal(registry.get("docs")?.id, created.id);
  assert.equal(registry.list().length, 1);
  assert.throws(() => registry.create("docs", [root]), /already exists/);

  const collection = registry.open("docs");
  assert.equal(registry.open("docs"), collection);
  assert.equal(collection.id, created.id);
  assert.equal(collection.name, "docs");
  const outside = collection.diagnoseFile(
    join(temporaryDirectory, "outside.ts"),
  );
  assert.equal(outside.belongsToCollection, false);
  assert.match(outside.reason, /outside/);
  const pending = collection.diagnoseFile(join(root, "alpha.ts"));
  assert.equal(pending.belongsToCollection, true);
  assert.match(pending.reason, /has not been indexed/);

  const indexed = await collection.index();
  assert.equal(indexed.filesAdded, 1);
  const indexedFile = collection.getFile(join(root, "alpha.ts"));
  assert.ok(indexedFile);
  assert.ok(collection.listEntitiesByFile(indexedFile.id).length > 0);
  const storedEntity = collection.listEntitiesByFile(indexedFile.id, {
    limit: 1,
    offset: 0,
  })[0];
  assert.equal(
    collection.getEntity(storedEntity.entity.id)?.file.id,
    indexedFile.id,
  );
  assert.equal((await registry.status("docs"))?.filesIndexed, 1);
  assert.equal(await registry.status("missing"), null);

  assert.throws(() => registry.rename("docs", " "), /non-empty name/);
  registry.create("other", [secondRoot]);
  assert.throws(() => registry.rename("docs", "other"), /already exists/);
  assert.equal(registry.rename("missing", "next"), null);
  const renamed = registry.rename("docs", "renamed");
  assert.equal(renamed?.name, "renamed");
  assert.equal(registry.has("docs"), false);
  assert.equal(registry.has("renamed"), true);

  const unchanged = registry.updateRootPaths("renamed", [root]);
  assert.equal(unchanged?.updatedTime, renamed?.updatedTime);
  const changed = registry.updateRootPaths("renamed", [secondRoot]);
  assert.equal(changed?.rootPaths[0].absolutePath, secondRoot);
  assert.equal(registry.updateRootPaths("missing", [root]), null);

  const disabled = registry.disableIndex("renamed", [secondRoot]);
  assert.equal(disabled.indexPolicy, "disabled");
  assert.equal(disabled.embedding, null);
  assert.equal(await registry.status("renamed"), null);
  assert.throws(() => registry.open("renamed"), /index is disabled/);
  const disabledNew = registry.disableIndex("disabled-new", [root]);
  assert.equal(disabledNew.indexPolicy, "disabled");

  const prepared = registry.prepareIndex("renamed", [root]);
  assert.equal(prepared.indexPolicy, "enabled");
  assert.equal(isCollectionIndexed(prepared), true);
  const createdByPrepare = registry.prepareIndex("prepared-new", [secondRoot]);
  assert.equal(createdByPrepare.name, "prepared-new");
  assert.throws(() => registry.open("missing"), /not found/);

  assert.equal(registry.remove("missing"), false);
  assert.equal(registry.remove("other"), true);
  assert.equal(registry.remove("disabled-new"), true);

  for (const [mutation, message] of [
    [{ indexVersion: 999 }, /version is not supported/],
    [{ embedding: null, indexVersion: null }, /has not been built/],
    [
      { embedding: { ...prepared.embedding, provider: "different" } },
      /provider does not match/,
    ],
    [
      { embedding: { ...prepared.embedding, model: "different" } },
      /model does not match/,
    ],
    [
      { embedding: { ...prepared.embedding, dimension: 999 } },
      /dimension does not match/,
    ],
    [
      { embedding: { ...prepared.embedding, metric: "dot" } },
      /metric does not match/,
    ],
  ]) {
    assert.throws(
      () =>
        new Collection(
          {
            ...prepared,
            path: join(home, `schema-${Math.random()}`),
            ...mutation,
          },
          embedding,
        ),
      message,
    );
  }

  registry.close();
  const readOnly = new CollectionRegistry(home, embedding, true);
  t.after(() => readOnly.close());
  assert.ok(readOnly.list().length > 0);
  for (const operation of [
    () => readOnly.create("readonly", [root]),
    () => readOnly.remove("renamed"),
    () => readOnly.rename("renamed", "readonly"),
    () => readOnly.prepareIndex("renamed", [root]),
    () => readOnly.disableIndex("renamed", [root]),
    () => readOnly.updateRootPaths("renamed", [root]),
  ]) {
    assert.throws(operation, /read-only/);
  }
  const readOnlyCollection = readOnly.open("renamed");
  assert.throws(() => readOnlyCollection.index(), /read-only/);
  readOnlyCollection.close();
});
