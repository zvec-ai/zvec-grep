import { appendFileSync, openSync, closeSync, readSync } from "node:fs";
import { StringDecoder } from "node:string_decoder";
import { dirname, join } from "node:path";
import { WorkspaceBindingStore } from "../bindings.js";
import { EngineError } from "../errors.js";
import {
  appendReservationCleanup,
  reserveDestination,
  assertDestinationAvailable,
  type DestinationReservation,
} from "../reservation.js";
import {
  computeLegacyIdentityRemaps,
  convertLegacyRootPaths,
  verifyConvertedIndex,
  rootCanonicalFromLegacy,
  type IndexConversionVerification,
  type LegacyManifest,
} from "../migrate/index.js";
import {
  assertHomeNotIncomplete,
  parsePortableManifest,
  readWorkspaceManifest,
  writeWorkspaceManifest,
} from "../manifest.js";
import { createFilesSchema, createEntitiesSchema } from "../storage/index.js";
import { createWorkspaceIndexStorage } from "../storage/index.js";
import { resolveWorkspaceIndexStoragePaths } from "../storage/layout.js";
import { readNativeTransferSource } from "../storage/transfer-source.js";
import { CURRENT_INDEX_VERSION } from "../types.js";
import { createCanonicalPathResolver } from "../utils/canonical-path.js";
import { readJsonFileSync, writeJsonFileSync } from "../utils/json.js";
import { acquireReadWriteLock } from "../utils/lock.js";
import {
  ZVecCreateAndOpen,
  ZVecInitialize,
  ZVecLogLevel,
  type ZVecCollection,
  type ZVecStatus,
} from "@zvec/zvec";

/**
 * Standalone logical transfer: export a workspace index (legacy v1 or
 * portable v2) to a versioned, credential-free artifact, and import such an
 * artifact into native storage on another host without opening the source
 * database. No embedding computation occurs on either side; imported indexes
 * carry no verification claim and reconcile at their first indexing run.
 */

export const TRANSFER_FORMAT = "zvec-grep-export";
export const TRANSFER_FORMAT_VERSION = 1;

const ENTITY_VECTOR_FIELD = "embedding";
const WRITE_CHUNK_LINES = 512;

export type TransferProgress = (stage: string, detail: string) => void;

export type ExportWorkspaceIndexOptions = {
  /** Current location of the index home (`<workspace>/.zvec-grep`). */
  sourceHome: string;
  /** Artifact directory to create; must not already exist. */
  artifactPath: string;
  onProgress?: TransferProgress;
};

export type ExportWorkspaceIndexResult = {
  artifactPath: string;
  indexId: string;
  filesExported: number;
  entitiesExported: number;
};

export type ImportWorkspaceIndexOptions = {
  artifactPath: string;
  /** Root of the destination workspace receiving the index. */
  destinationRoot: string;
  /** Maximum number of entities compared vector-by-vector (0 = all). */
  verifySampleLimit?: number;
  onProgress?: TransferProgress;
};

export type ImportWorkspaceIndexResult = {
  destinationHome: string;
  indexId: string;
  filesImported: number;
  entitiesImported: number;
  missingFiles: string[];
  verification: IndexConversionVerification;
};

type TransferFormatFile = {
  format: typeof TRANSFER_FORMAT;
  formatVersion: number;
  indexId: string;
  embedding: {
    provider: string;
    model: string;
    dimension: number;
    metric: "cosine" | "dot" | "euclidean";
  };
  indexVersion: number;
  createdTime: number;
  exportedTime: number;
  counts: { files: number; entities: number };
};

type PortableManifest = Parameters<typeof writeWorkspaceManifest>[1];

export async function exportWorkspaceIndex(
  options: ExportWorkspaceIndexOptions,
): Promise<ExportWorkspaceIndexResult> {
  const report = options.onProgress ?? (() => undefined);
  const sourceHome = options.sourceHome;

  // Writer exclusion across the manifest and both collections, acquired
  // before any read.
  const lock = acquireReadWriteLock(join(sourceHome, "locks", "home"), "read", {
    operation: "index.export",
  });
  let reservation: DestinationReservation | undefined;
  let source: ReturnType<typeof readNativeTransferSource> | undefined;
  try {
    // The incomplete-home guard runs under the source lock before any
    // collection or metadata read: a crashed reservation blocks consumption
    // even after its owner is gone.
    assertHomeNotIncomplete(sourceHome);
    assertDestinationAvailable(options.artifactPath, [
      "format.json",
      "manifest.json",
    ]);
    report("read", "Reading source index");
    ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
    source = readNativeTransferSource(sourceHome);
    const counts = source.counts;

    const rawManifest = readJsonFileSync<unknown>(
      join(sourceHome, "manifest.json"),
      null,
    );
    if (rawManifest === null) {
      throw transferError("Index manifest not found", sourceHome);
    }

    report("write", "Writing transfer artifact");
    // The artifact directory is reserved exclusively: competing operations
    // meet the lock, and an aborted export removes only its own staging.
    const reserved = reserveDestination({
      destinationHome: options.artifactPath,
      operation: "index.export",
      existingIndexMarkers: ["format.json", "manifest.json"],
    });
    reservation = reserved;
    const writer = artifactWriter(reserved.stagingHome);

    let portableManifest: PortableManifest;
    let embedding: TransferFormatFile["embedding"];
    if (
      isRecord(rawManifest) &&
      rawManifest.manifestVersion === 1 &&
      typeof rawManifest.id === "string" &&
      typeof rawManifest.path === "string" &&
      isRecord(rawManifest.embedding)
    ) {
      // Legacy source: remap identities into portable form.
      const originalRoot = dirname(rawManifest.path);
      const { fileIdByOld, canonicalByOldFileId, fragmentIdByOld } =
        computeLegacyIdentityRemaps(
          originalRoot,
          rawManifest.id,
          source.files(),
          source.entities(false),
        );
      for (const doc of source.files()) {
        const { absolute_path: _legacyAbsolute, ...fields } = doc.fields;
        const newId = fileIdByOld.get(doc.id)!;
        writer.writeDoc("files", {
          id: newId,
          fields: {
            ...fields,
            file_id: newId,
            canonical_path: canonicalByOldFileId.get(doc.id)!,
            root_path: rootCanonicalFromLegacy(
              String(doc.fields.root_path ?? ""),
              originalRoot,
            ),
            entity_ids_json: JSON.stringify(
              (
                JSON.parse(
                  String(doc.fields.entity_ids_json ?? "[]"),
                ) as string[]
              ).map((old) => fragmentIdByOld.get(old) ?? old),
            ),
          },
        });
      }
      for (const doc of source.entities()) {
        const group = doc.fields.group;
        writer.writeDoc("entities", {
          id: fragmentIdByOld.get(doc.id)!,
          fields: {
            ...doc.fields,
            file_id: fileIdByOld.get(String(doc.fields.file_id ?? ""))!,
            ...(typeof group === "string" && group.length > 0
              ? { group: fragmentIdByOld.get(group) ?? group }
              : {}),
          },
          vector: vectorToBase64(doc.vectors[ENTITY_VECTOR_FIELD]),
        });
      }
      embedding = rawManifest.embedding as TransferFormatFile["embedding"];
      portableManifest = portableManifestFromLegacy(rawManifest, originalRoot);
    } else {
      const manifest = readWorkspaceManifest(sourceHome);
      if (!manifest) {
        throw transferError("Index manifest not found", sourceHome);
      }
      if (!manifest.embedding) {
        throw transferError(
          "Workspace has no built index to export",
          sourceHome,
        );
      }
      for (const doc of source.files()) {
        writer.writeDoc("files", { id: doc.id, fields: { ...doc.fields } });
      }
      for (const doc of source.entities()) {
        writer.writeDoc("entities", {
          id: doc.id,
          fields: { ...doc.fields },
          vector: vectorToBase64(doc.vectors[ENTITY_VECTOR_FIELD]),
        });
      }
      embedding = manifest.embedding as TransferFormatFile["embedding"];
      portableManifest = manifest;
    }

    writer.finish();
    source.close();
    source = undefined;
    report("publish", "Publishing transfer artifact");
    // Publication: format and manifest metadata are finalized into staging
    // and moved into place last; the reservation commits once at release.
    reserved.publish(() => {
      const formatFile: TransferFormatFile = {
        format: TRANSFER_FORMAT,
        formatVersion: TRANSFER_FORMAT_VERSION,
        indexId: portableManifest.id,
        embedding,
        indexVersion: CURRENT_INDEX_VERSION,
        createdTime: portableManifest.createdTime,
        exportedTime: Date.now(),
        counts,
      };
      writeJsonFileSync(join(reserved.stagingHome, "format.json"), formatFile, {
        fileMode: 0o600,
      });
      writeJsonFileSync(
        join(reserved.stagingHome, "manifest.json"),
        portableManifest,
        { fileMode: 0o600 },
      );
    });
    report("done", "Export complete");

    return {
      artifactPath: options.artifactPath,
      indexId: portableManifest.id,
      filesExported: counts.files,
      entitiesExported: counts.entities,
    };
  } catch (error) {
    const cleanup = reservation?.abort();
    if (cleanup) {
      throw appendReservationCleanup(error, cleanup);
    }
    throw error;
  } finally {
    try {
      source?.close();
    } finally {
      lock.release();
    }
  }
}

export async function importWorkspaceIndex(
  options: ImportWorkspaceIndexOptions,
): Promise<ImportWorkspaceIndexResult> {
  const report = options.onProgress ?? (() => undefined);
  const artifactPath = options.artifactPath;
  const destinationRoot = options.destinationRoot;

  // The artifact's read lock is held for the whole operation: consumers
  // respect the exporter's release-as-commit boundary, and no writer may
  // change the artifact while it is consumed.
  const artifactLock = acquireReadWriteLock(
    join(artifactPath, "locks", "home"),
    "read",
    { operation: "index.import" },
  );
  try {
    // A crashed export leaves a durable INCOMPLETE marker behind even after
    // its lock is gone; such artifacts are rejected before any metadata read
    // or destination staging.
    assertHomeNotIncomplete(artifactPath);
    return await importWorkspaceIndexLocked(
      options,
      report,
      artifactPath,
      destinationRoot,
    );
  } finally {
    artifactLock.release();
  }
}

async function importWorkspaceIndexLocked(
  options: ImportWorkspaceIndexOptions,
  report: (stage: string, detail: string) => void,
  artifactPath: string,
  destinationRoot: string,
): Promise<ImportWorkspaceIndexResult> {
  assertDestinationAvailable(join(destinationRoot, ".zvec-grep"), [
    "manifest.json",
    "files.zvec",
    "index.zvec",
  ]);
  report("read", "Reading transfer artifact");
  const formatFile = readJsonFileSync<TransferFormatFile | null>(
    join(artifactPath, "format.json"),
    null,
  );
  if (
    !formatFile ||
    formatFile.format !== TRANSFER_FORMAT ||
    formatFile.formatVersion !== TRANSFER_FORMAT_VERSION
  ) {
    throw transferError(
      "Transfer artifact is missing or has an unsupported format version",
      artifactPath,
    );
  }
  if (formatFile.indexVersion !== CURRENT_INDEX_VERSION) {
    throw transferError(
      "Transfer artifact uses an unsupported index version",
      artifactPath,
    );
  }
  const rawManifest = readJsonFileSync<unknown>(
    join(artifactPath, "manifest.json"),
    null,
  );
  if (rawManifest === null) {
    throw transferError("Transfer artifact manifest is missing", artifactPath);
  }
  // Validate and reconstruct portable metadata from the allowlist before
  // anything is staged: invalid, credential-bearing, or legacy manifests
  // leave the artifact and any existing destination untouched.
  let portableManifest: PortableManifest;
  try {
    portableManifest = parsePortableManifest(rawManifest, artifactPath);
  } catch {
    throw transferError(
      "Transfer artifact manifest is invalid or unsupported",
      artifactPath,
    );
  }
  if (formatFile.indexId !== portableManifest.id) {
    throw transferError(
      "Transfer artifact identity does not match its manifest",
      artifactPath,
    );
  }
  if (portableManifest.indexVersion !== CURRENT_INDEX_VERSION) {
    throw transferError(
      "Transfer artifact manifest uses an unsupported index version",
      artifactPath,
    );
  }
  if (
    !portableManifest.embedding ||
    portableManifest.embedding.provider !== formatFile.embedding.provider ||
    portableManifest.embedding.model !== formatFile.embedding.model ||
    portableManifest.embedding.dimension !== formatFile.embedding.dimension ||
    portableManifest.embedding.metric !== formatFile.embedding.metric
  ) {
    throw transferError(
      "Transfer artifact embedding schema does not match its manifest",
      artifactPath,
    );
  }

  const destinationHome = join(destinationRoot, ".zvec-grep");
  const reserved = reserveDestination({
    destinationHome,
    operation: "index.import",
    existingIndexMarkers: ["manifest.json", "files.zvec", "index.zvec"],
  });

  const openHandles: ZVecCollection[] = [];
  try {
    report("write", "Building destination index");
    ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
    const stagingPaths = resolveWorkspaceIndexStoragePaths(
      reserved.stagingHome,
    );
    const destFiles = track(
      ZVecCreateAndOpen(stagingPaths.filesPath, createFilesSchema()),
    );
    const destEntities = track(
      ZVecCreateAndOpen(
        stagingPaths.indexPath,
        createEntitiesSchema(formatFile.embedding),
      ),
    );

    const files = () =>
      readJsonLines<{ id: string; fields: Record<string, unknown> }>(
        join(artifactPath, "files.jsonl"),
      );
    const entities = () =>
      readJsonLines<{
        id: string;
        fields: Record<string, unknown>;
        vector: string;
      }>(join(artifactPath, "entities.jsonl"));
    let fileCount = 0;
    let entityCount = 0;
    let probe: number[] | undefined;
    const resolver = createCanonicalPathResolver(destinationRoot);
    const missingFiles: string[] = [];
    for (const doc of files()) {
      fileCount++;
      assertInsertOk(
        destFiles.insertSync({ id: doc.id, fields: doc.fields }),
        doc.id,
      );
      const canonicalPath = String(doc.fields.canonical_path ?? "");
      const existence = resolver.resolveDetailedSync(canonicalPath);
      if (existence.status === "forbidden") {
        throw transferError(
          "Destination file escapes the workspace through a symlink",
          `canonicalPath=${canonicalPath} resolved=${existence.path}`,
        );
      }
      if (existence.status === "missing") {
        missingFiles.push(canonicalPath);
      }
    }
    for (const doc of entities()) {
      entityCount++;
      const vector = base64ToVector(doc.vector);
      probe ??= Array.from(vector);
      assertInsertOk(
        destEntities.insertSync({
          id: doc.id,
          fields: doc.fields,
          vectors: { [ENTITY_VECTOR_FIELD]: vector },
        }),
        doc.id,
      );
    }
    if (
      fileCount !== formatFile.counts.files ||
      entityCount !== formatFile.counts.entities
    ) {
      throw transferError(
        "Transfer artifact counts do not match its format declaration",
        artifactPath,
      );
    }
    closeTracked(destFiles);
    closeTracked(destEntities);

    report("verify", "Verifying destination index");
    const verification = verifyConvertedIndex(
      stagingPaths,
      { files: fileCount, entities: entityCount },
      (function* () {
        for (const doc of entities())
          yield [doc.id, base64ToVector(doc.vector)] as const;
      })(),
      options.verifySampleLimit ?? 256,
      portableManifest.id,
      formatFile.embedding,
    );
    if (
      !verification.countsMatch ||
      !verification.identitiesUnique ||
      !verification.identitiesDerived ||
      !verification.requiredFieldsValid ||
      !verification.ownershipValid ||
      !verification.inventoriesExact ||
      !verification.groupIntegrity ||
      !verification.vectorsPreserved
    ) {
      throw transferError(
        "Imported destination failed verification",
        JSON.stringify(verification),
      );
    }

    // The staged index must also answer through the application's normal
    // readers — without model inference or automatic indexing.
    report("verify", "Checking staged index through normal readers");
    const stagedStorage = createWorkspaceIndexStorage({
      storagePath: reserved.stagingHome,
      workspaceRoot: destinationRoot,
      readOnly: true,
    });
    try {
      const stagedFiles = stagedStorage.listFiles();
      if (stagedFiles.length !== fileCount) {
        throw transferError(
          "Staged index file count does not match through normal readers",
          artifactPath,
        );
      }
      if (probe) {
        const hits = stagedStorage.searchVector(
          probe,
          Math.min(5, entityCount),
        );
        if (hits.length === 0) {
          throw transferError(
            "Staged index returns no results through normal search",
            artifactPath,
          );
        }
      }
    } finally {
      stagedStorage.close();
    }

    // Supported replacement workflow: verification is invalidated before
    // publication releases its reservation, never after the commit window.
    report("publish", "Publishing verified index");
    new WorkspaceBindingStore().invalidate(
      portableManifest.id,
      destinationRoot,
    );
    // Publication: finalize the manifest into staging, move children with
    // the manifest last, and commit once at reservation release.
    reserved.publish(() =>
      writeWorkspaceManifest(reserved.stagingHome, {
        ...portableManifest,
        updatedTime: Date.now(),
      }),
    );
    report("done", "Import complete");

    return {
      destinationHome,
      indexId: portableManifest.id,
      filesImported: fileCount,
      entitiesImported: entityCount,
      missingFiles,
      verification,
    };
  } catch (error) {
    // Native handles close before any filesystem cleanup.
    for (const handle of openHandles.splice(0).reverse()) {
      try {
        handle.closeSync();
      } catch {
        // Cleanup is best-effort.
      }
    }
    const cleanup = reserved.abort();
    if (cleanup) {
      throw appendReservationCleanup(error, cleanup);
    }
    throw error;
  }

  function track(collection: ZVecCollection): ZVecCollection {
    openHandles.push(collection);
    return collection;
  }

  function closeTracked(collection: ZVecCollection): void {
    const index = openHandles.indexOf(collection);
    if (index >= 0) {
      openHandles.splice(index, 1);
    }
    collection.closeSync();
  }
}

function artifactWriter(artifactPath: string) {
  const buffers: Record<"files" | "entities", string[]> = {
    files: [],
    entities: [],
  };
  return {
    writeDoc(kind: "files" | "entities", doc: unknown): void {
      buffers[kind].push(JSON.stringify(doc));
      if (buffers[kind].length >= WRITE_CHUNK_LINES) {
        appendFileSync(
          join(artifactPath, `${kind}.jsonl`),
          buffers[kind].join("\n") + "\n",
        );
        buffers[kind] = [];
      }
    },
    finish(): void {
      for (const kind of ["files", "entities"] as const) {
        if (buffers[kind].length > 0) {
          appendFileSync(
            join(artifactPath, `${kind}.jsonl`),
            buffers[kind].join("\n") + "\n",
          );
          buffers[kind] = [];
        }
      }
    },
  };
}

// A bounded line reader keeps import independent of total artifact size.
// StringDecoder preserves UTF-8 characters split across disk reads.
function* readJsonLines<T>(path: string): Generator<T> {
  const fd = openSync(path, "r");
  const decoder = new StringDecoder("utf8");
  const buffer = Buffer.alloc(64 * 1024);
  let pending = "";
  try {
    for (;;) {
      const size = readSync(fd, buffer);
      pending += size ? decoder.write(buffer.subarray(0, size)) : decoder.end();
      let end: number;
      while ((end = pending.indexOf("\n")) !== -1) {
        const line = pending.slice(0, end);
        pending = pending.slice(end + 1);
        if (line.trim()) yield JSON.parse(line) as T;
      }
      if (pending.length > 64 * 1024 * 1024) {
        throw transferError("Transfer artifact record exceeds 64 MiB", path);
      }
      if (!size) break;
    }
    if (pending.trim()) yield JSON.parse(pending) as T;
  } finally {
    closeSync(fd);
  }
}

function portableManifestFromLegacy(
  raw: Record<string, unknown>,
  originalRoot: string,
): PortableManifest {
  return {
    manifestVersion: 2,
    id: raw.id as string,
    name: raw.name as string,
    rootPaths: convertLegacyRootPaths(
      raw.rootPaths as LegacyManifest["rootPaths"],
      originalRoot,
    ),
    indexPolicy: raw.indexPolicy as PortableManifest["indexPolicy"],
    embedding: raw.embedding as PortableManifest["embedding"],
    indexVersion: CURRENT_INDEX_VERSION,
    createdTime: raw.createdTime as number,
    updatedTime: Date.now(),
    embeddingRuntime: {
      ...(typeof (raw.embeddingRuntime as Record<string, unknown>)?.endpoint ===
      "string"
        ? {
            endpoint: (raw.embeddingRuntime as Record<string, unknown>)
              .endpoint as string,
          }
        : {}),
    },
  };
}

function vectorToBase64(vector: unknown): string {
  if (Array.isArray(vector)) {
    return Buffer.from(new Float32Array(vector.map(Number)).buffer).toString(
      "base64",
    );
  }
  if (ArrayBuffer.isView(vector)) {
    const view = vector as Float32Array;
    return Buffer.from(
      view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength),
    ).toString("base64");
  }
  throw transferError("Entity is missing vector data", "vector");
}

function base64ToVector(encoded: string): Float32Array {
  const buffer = Buffer.from(encoded, "base64");
  return new Float32Array(
    buffer.buffer.slice(
      buffer.byteOffset,
      buffer.byteOffset + buffer.byteLength,
    ),
  );
}

function assertInsertOk(status: ZVecStatus, id: string): void {
  if (!status.ok) {
    throw transferError(
      "Native write failed during destination build",
      `id=${id} code=${status.code} message=${status.message}`,
    );
  }
}

function transferError(message: string, context: string): EngineError {
  return new EngineError(`Workspace index transfer failed: ${message}`, {
    code: "ZVEC_GREP.ENGINE.TRANSFER.FAILED",
    context,
  });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
