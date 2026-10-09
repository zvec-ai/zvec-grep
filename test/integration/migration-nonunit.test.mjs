import assert from "node:assert/strict";
import { mkdir, writeFile, readFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { migrateWorkspaceIndex } from "../../dist/engine/migrate/index.js";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../../dist/engine/transfer/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";

useIsolatedZvecGrepHome();

class NonunitModel extends FakeEmbeddingModel {
  constructor() {
    super();
    this.info = { ...this.info, dimension: 1024 };
    this.documents = 0;
  }
  async doEmbed(contents, options) {
    if (options?.purpose !== "query") this.documents += contents.length;
    return {
      vectors: contents.map((content) => {
        let state = [...content.text].reduce(
          (sum, c) => sum + c.charCodeAt(0),
          1,
        );
        return Array.from({ length: 1024 }, () => {
          state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
          return Math.fround((state / 2 ** 32 - 0.5) * 12);
        });
      }),
      truncated: [],
    };
  }
}

for (const operation of ["migrate", "export-import"]) {
  test(`${operation} preserves non-unit 1024-dimensional cosine vectors`, async (t) => {
    const parent = await createTemporaryDirectory(t, "zg-nonunit-");
    const root = join(parent, "source");
    await mkdir(root);
    for (let i = 0; i < 128; i++) {
      await writeFile(
        join(root, `${i}.md`),
        `# Beacon ${i}\n\nnonunit migration sentinel ${i}\n`,
      );
    }
    const model = new NonunitModel();
    const service = await createZvecGrep({ root, embeddingModel: model });
    try {
      await service.index();
    } finally {
      await service.close();
    }
    const calls = model.documents;
    const manifest = JSON.parse(
      await readFile(join(root, ".zvec-grep/manifest.json"), "utf8"),
    );
    const sourceHome = join(root, ".legacy");
    await buildLegacyHome(root, sourceHome, manifest.id, {
      embedding: manifest.embedding,
    });
    const destinationRoot = join(parent, "destination");
    await mkdir(destinationRoot);
    for (let i = 0; i < 128; i++) {
      await writeFile(
        join(destinationRoot, `${i}.md`),
        await readFile(join(root, `${i}.md`)),
      );
    }
    const result =
      operation === "migrate"
        ? await migrateWorkspaceIndex({
            sourceHome,
            destinationRoot,
            verifySampleLimit: 0,
          })
        : await (async () => {
            const artifactPath = join(parent, "artifact");
            await exportWorkspaceIndex({ sourceHome, artifactPath });
            return importWorkspaceIndex({
              artifactPath,
              destinationRoot,
              verifySampleLimit: 0,
            });
          })();
    assert.equal(result.indexId, manifest.id);
    assert.equal(result.verification.vectorsPreserved, true);
    assert.equal(result.verification.vectorsSampled, false);
    assert.ok(result.verification.vectorsCompared >= 128);
    assert.equal(model.documents, calls);
    const reopened = await createZvecGrep({
      root: destinationRoot,
      embeddingModel: model,
    });
    try {
      const found = await reopened.context({
        query: "nonunit migration sentinel",
        limit: 3,
      });
      assert.ok(
        found.items.some((hit) =>
          hit.content.includes("nonunit migration sentinel"),
        ),
      );
      assert.equal(model.documents, calls, "search must not embed documents");
    } finally {
      await reopened.close();
    }
  });
}
