import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { ZVecCreateAndOpen, ZVecInitialize, ZVecLogLevel } from "@zvec/zvec";
import { createEntitiesSchema } from "../../dist/engine/storage/index.js";
import { legacyFilesSchema } from "./legacy-index.mjs";

// Generate one vector at a time. Building the fixture must not require the
// all-record allocation whose removal the regression is intended to prove.
export async function largeLegacy(parent, count) {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const root = join(parent, "source");
  const sourceHome = join(root, ".zvec-grep");
  await mkdir(sourceHome, { recursive: true });
  const embedding = {
    provider: "test",
    model: "nonunit",
    dimension: 1024,
    metric: "cosine",
  };
  const hash = (value) => createHash("sha256").update(value).digest("hex");
  const fileId = hash("legacy-file");
  const ids = Array.from({ length: count }, (_, i) =>
    hash(`legacy-fragment-${i}`),
  );
  const files = ZVecCreateAndOpen(
    join(sourceHome, "files.zvec"),
    legacyFilesSchema(),
  );
  try {
    assert.ok(
      files.insertSync({
        id: fileId,
        fields: {
          file_id: fileId,
          absolute_path: join(root, "beacon.md"),
          relative_path: "beacon.md",
          root_path: root,
          size_bytes: 42,
          last_modified_time: 1000,
          kind: "text",
          format: "markdown",
          has_index_status: true,
          indexed_time: 1000,
          entity_count: count,
          entity_ids_json: JSON.stringify(ids),
        },
      }).ok,
    );
  } finally {
    files.closeSync();
  }
  const entities = ZVecCreateAndOpen(
    join(sourceHome, "index.zvec"),
    createEntitiesSchema(embedding),
  );
  try {
    for (let i = 0; i < count; i++) {
      let state = i + 1;
      const vector = Array.from({ length: 1024 }, () => {
        state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
        return Math.fround((state / 2 ** 32 - 0.5) * 12);
      });
      assert.ok(
        entities.insertSync({
          id: ids[i],
          fields: {
            file_id: fileId,
            fragment_index: i,
            range_json: '{"kind":"file"}',
            content_kind: "text",
            text: `migration memory sentinel ${i}`,
          },
          vectors: { embedding: vector },
        }).ok,
      );
    }
  } finally {
    entities.closeSync();
  }
  const manifest = {
    manifestVersion: 1,
    id: "c6dca33e-1998-486c-8f06-4c2ea3dacd5b",
    name: "large-legacy",
    path: sourceHome,
    rootPaths: [
      {
        absolutePath: root,
        recursive: true,
        include: ["*.md"],
        globs: ["!private/**"],
      },
    ],
    indexPolicy: "enabled",
    embedding,
    indexVersion: 1,
    createdTime: 1000,
    updatedTime: 1000,
    embeddingRuntime: {},
  };
  await writeFile(join(sourceHome, "manifest.json"), JSON.stringify(manifest));
  return { sourceHome, manifest };
}
