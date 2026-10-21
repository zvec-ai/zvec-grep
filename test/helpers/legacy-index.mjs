import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import {
  ZVecCollectionSchema,
  ZVecCreateAndOpen,
  ZVecDataType,
  ZVecIndexType,
  ZVecOpen,
} from "@zvec/zvec";
import { createEntitiesSchema } from "../../dist/engine/storage/index.js";
import { resolveWorkspaceIndexStoragePaths } from "../../dist/engine/storage/layout.js";

export const LEGACY_EMBEDDING = {
  provider: "test",
  model: "deterministic",
  dimension: 16,
  metric: "cosine",
};

function oldFileId(indexId, absolutePath) {
  return createHash("sha256")
    .update(`${indexId}\0${resolve(absolutePath)}`)
    .digest("hex");
}

function oldFragmentId(oldId, fragmentIndex) {
  return createHash("sha256")
    .update(`${oldId}\0${fragmentIndex}`)
    .digest("hex");
}

function indexedString(name, nullable = false) {
  return {
    name,
    dataType: ZVecDataType.STRING,
    nullable,
    indexParams: { indexType: ZVecIndexType.INVERT },
  };
}

function plainString(name, nullable = false) {
  return { name, dataType: ZVecDataType.STRING, nullable };
}

export function legacyFilesSchema() {
  return new ZVecCollectionSchema({
    name: "zvec_grep_files",
    fields: [
      indexedString("file_id"),
      indexedString("absolute_path"),
      plainString("relative_path"),
      plainString("root_path"),
      { name: "size_bytes", dataType: ZVecDataType.INT64, nullable: false },
      {
        name: "last_modified_time",
        dataType: ZVecDataType.INT64,
        nullable: false,
      },
      indexedString("content_hash", true),
      indexedString("kind"),
      indexedString("format"),
      {
        name: "has_index_status",
        dataType: ZVecDataType.BOOL,
        nullable: false,
      },
      { name: "indexed_time", dataType: ZVecDataType.INT64, nullable: true },
      { name: "entity_count", dataType: ZVecDataType.INT32, nullable: false },
      { name: "token_count", dataType: ZVecDataType.INT32, nullable: true },
      {
        name: "truncated_fragment_count",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      plainString("error", true),
      plainString("entity_ids_json"),
    ],
  });
}

// Build a legacy (v1) index home from a real v2 index: identical content in
// the legacy absolute-path format, including a persisted credential/device.
// mutateFileIds: optional map from v2 file doc id to a function transforming
// that file's entity_ids_json (used to inject corrupt inventories).
export async function buildLegacyHome(
  v2Root,
  legacyHome,
  indexId,
  { mutateInventory, embedding = LEGACY_EMBEDDING } = {},
) {
  const v2Paths = resolveWorkspaceIndexStoragePaths(join(v2Root, ".zvec-grep"));
  const srcFiles = ZVecOpen(v2Paths.filesPath, { readOnly: true });
  const srcEntities = ZVecOpen(v2Paths.indexPath, { readOnly: true });
  const fileDocs = [...srcFiles.iterDocsSync({ includeVector: false })];
  const entityDocs = [...srcEntities.iterDocsSync({ includeVector: true })];

  const legacyPaths = resolveWorkspaceIndexStoragePaths(legacyHome);
  await mkdir(legacyPaths.storagePath, { recursive: true });
  const dstFiles = ZVecCreateAndOpen(
    legacyPaths.filesPath,
    legacyFilesSchema(),
  );
  const dstEntities = ZVecCreateAndOpen(
    legacyPaths.indexPath,
    createEntitiesSchema(embedding),
  );

  const legacyFileIds = new Map();
  for (const doc of fileDocs) {
    const absolutePath = join(v2Root, doc.fields.canonical_path);
    legacyFileIds.set(doc.id, oldFileId(indexId, absolutePath));
  }
  const legacyFragmentIds = new Map();
  for (const doc of entityDocs) {
    const oldFile = legacyFileIds.get(String(doc.fields.file_id));
    legacyFragmentIds.set(
      doc.id,
      oldFragmentId(oldFile, Number(doc.fields.fragment_index)),
    );
  }

  for (const doc of fileDocs) {
    const absolutePath = join(v2Root, doc.fields.canonical_path);
    let entityIds = JSON.parse(doc.fields.entity_ids_json).map((id) =>
      legacyFragmentIds.get(id),
    );
    if (mutateInventory) {
      entityIds = mutateInventory(entityIds, {
        fileId: doc.id,
        legacyFragmentIds,
      });
    }
    const legacyFields = { ...doc.fields };
    delete legacyFields.canonical_path;
    dstFiles.insertSync({
      id: legacyFileIds.get(doc.id),
      fields: {
        ...legacyFields,
        file_id: legacyFileIds.get(doc.id),
        absolute_path: resolve(absolutePath),
        root_path: resolve(v2Root),
        entity_ids_json: JSON.stringify(entityIds),
      },
    });
  }
  for (const doc of entityDocs) {
    const group = doc.fields.group;
    dstEntities.insertSync({
      id: legacyFragmentIds.get(doc.id),
      vectors: { embedding: doc.vectors.embedding },
      fields: {
        ...doc.fields,
        file_id: legacyFileIds.get(String(doc.fields.file_id)),
        ...(typeof group === "string" && group.length > 0
          ? { group: legacyFragmentIds.get(group) }
          : {}),
      },
    });
  }
  dstFiles.closeSync();
  dstEntities.closeSync();
  srcFiles.closeSync();
  srcEntities.closeSync();

  await writeFile(
    join(legacyHome, "manifest.json"),
    JSON.stringify({
      manifestVersion: 1,
      id: indexId,
      name: "legacy-workspace",
      path: legacyHome,
      rootPaths: [{ absolutePath: resolve(v2Root), recursive: true }],
      indexPolicy: "enabled",
      embedding,
      indexVersion: 1,
      createdTime: 1000,
      updatedTime: 1000,
      embeddingRuntime: {
        apiKey: "legacy-persisted-secret",
        endpoint: "http://127.0.0.1:9/embeddings",
        device: "metal",
      },
    }),
    { mode: 0o600 },
  );
  return {
    fileCount: fileDocs.length,
    entityCount: entityDocs.length,
    fileDocs,
    entityDocs,
    legacyFragmentIds,
    legacyFileIds,
  };
}
