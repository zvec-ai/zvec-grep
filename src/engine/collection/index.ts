import {
  ZVecCollectionSchema,
  ZVecCreateAndOpen,
  ZVecDataType,
  ZVecIndexType,
  ZVecOpen,
  type ZVecCollection,
  type ZVecDoc,
  type ZVecDocInput,
} from "@zvec/zvec";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import {
  collectionDetail,
  detail,
  EngineError,
  errorDetails,
} from "../errors/index.js";
import type { EmbeddingModel } from "../models/index.js";
import {
  getCollectionIndexStatus,
  indexCollection,
  indexCollectionPaths,
} from "../pipeline/indexing/index.js";
import {
  fileBelongsToRootPath,
  validateRootPaths,
} from "../pipeline/indexing/root-paths.js";
import {
  diagnoseEntitySearch,
  diagnoseFileSearch,
  searchPlanCollection,
} from "../pipeline/search/index.js";
import type { CollectionStorage, StoredEntity } from "../storage/index.js";
import {
  assertZvecStatus,
  initializeZvec,
  openZvecCollection,
  quoteFilterString,
  readNullableNumberFieldFromFields,
  readNullableStringFieldFromFields,
  readNumberFieldFromFields,
  readStringFieldFromFields,
  ZvecCollectionStorage,
  ZvecFileMetaStore,
} from "../storage/zvec.js";
import type {
  CollectionEmbeddingSchema,
  CollectionIndexStatus,
  CollectionInfo,
  CollectionIndexPolicy,
  EntitySearchDiagnosis,
  FileDiagnosis,
  FileInfo,
  IndexOptions,
  IndexResult,
  RootPath,
  SearchPlan,
  SearchPlanResult,
} from "../types.js";
import { CURRENT_INDEX_VERSION } from "../types.js";
import { defaultHome, normalizePath } from "../utils/path.js";

const COLLECTIONS_ZVEC = "collections.zvec";
const FILES_ZVEC = "files.zvec";

export class Collection {
  private readonly storage: CollectionStorage;
  private readonly embedding: CollectionEmbeddingSchema;
  private closed = false;

  constructor(
    readonly info: CollectionInfo,
    private readonly embeddingModel?: EmbeddingModel,
    private readonly readOnly = false,
    private readonly fileStorePath = join(info.path, FILES_ZVEC),
  ) {
    this.embedding = requireIndexedEmbedding(info, "open");
    this.validateIndexVersion();
    if (this.embeddingModel) {
      this.validateEmbeddingSchema(this.embeddingModel);
    }

    this.storage = new ZvecCollectionStorage(
      info.path,
      this.embedding,
      this.fileStorePath,
      info.id,
      readOnly,
    );
  }

  get id(): string {
    return this.info.id;
  }

  get name(): string {
    return this.info.name;
  }

  index(options: IndexOptions = {}): Promise<IndexResult> {
    if (this.readOnly) {
      throw new EngineError("Cannot index a read-only collection", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
        context: collectionOperationDetails(this.name, "index"),
      });
    }

    const embeddingModel = this.requireEmbeddingModel("index");

    const context = {
      collection: this.info,
      embeddingModel,
      storage: this.storage,
      embeddingConcurrency: options.embeddingConcurrency,
      onProgress: options.onProgress,
      signal: options.signal,
    };
    return options.changedPaths && options.changedPaths.length > 0
      ? indexCollectionPaths(context, options.changedPaths)
      : indexCollection(context);
  }

  status(): Promise<CollectionIndexStatus> {
    return getCollectionIndexStatus(this.info, this.storage.listFiles());
  }

  searchPlan(plan: SearchPlan): Promise<SearchPlanResult> {
    return searchPlanCollection(plan, {
      collection: this.info,
      embeddingModel: this.embeddingModel,
      storage: this.storage,
    });
  }

  diagnoseFile(absolutePath: string): FileDiagnosis {
    const path = normalizePath(absolutePath);
    const file = this.storage.getFileByPath(path);
    const matchedRootPath = this.info.rootPaths.find((rootPath) =>
      fileBelongsToRootPath(path, rootPath),
    );

    return {
      collectionId: this.id,
      collectionName: this.name,
      absolutePath: path,
      belongsToCollection: matchedRootPath !== undefined,
      matchedRootPath: matchedRootPath?.absolutePath,
      file: file ?? undefined,
      entityCount: file?.indexStatus?.entityCount ?? 0,
      reason: file
        ? undefined
        : matchedRootPath
          ? "File is under a collection root but has not been indexed; file-selection, ignore, discovery, size, or file-type rules may exclude it"
          : "File is outside the collection roots or excluded by root include/exclude patterns",
    };
  }

  diagnoseEntitySearch(
    query: string,
    entityId: string,
  ): Promise<EntitySearchDiagnosis> {
    const embeddingModel = this.requireEmbeddingModel("diagnose");

    return diagnoseEntitySearch(query, entityId, {
      collection: this.info,
      embeddingModel,
      storage: this.storage,
    });
  }

  diagnoseFileSearch(
    query: string,
    absolutePath: string,
  ): Promise<EntitySearchDiagnosis | null> {
    const embeddingModel = this.requireEmbeddingModel("diagnose");

    return diagnoseFileSearch(query, absolutePath, {
      collection: this.info,
      embeddingModel,
      storage: this.storage,
    });
  }

  getFile(absolutePath: string): FileInfo | null {
    return this.storage.getFileByPath(absolutePath);
  }

  listEntitiesByFile(
    fileId: string,
    options: { limit?: number; offset?: number } = {},
  ): StoredEntity[] {
    return this.storage.listEntitiesByFile(fileId, options);
  }

  getEntity(entityId: string): StoredEntity | null {
    return this.storage.getEntity(entityId);
  }

  close(): void {
    if (this.closed) {
      return;
    }

    this.storage.close();
    this.closed = true;
  }

  private validateIndexVersion(): void {
    if (this.info.indexVersion !== CURRENT_INDEX_VERSION) {
      throw new EngineError("Collection index version is not supported", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.INDEX_VERSION_MISMATCH",
        context: errorDetails([
          collectionDetail(this.name),
          detail("expected", CURRENT_INDEX_VERSION),
          detail("actual", this.info.indexVersion),
        ]),
      });
    }
  }

  private validateEmbeddingSchema(current: EmbeddingModel): void {
    this.validateIndexVersion();

    const expected = this.embedding;

    if (expected.provider !== current.info.provider) {
      throw new EngineError(
        "Collection embedding provider does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_PROVIDER_MISMATCH",
          context: collectionMismatchDetails(
            this.name,
            expected.provider,
            current.info.provider,
          ),
        },
      );
    }

    if (expected.model !== current.info.name) {
      throw new EngineError(
        "Collection embedding model does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_MODEL_MISMATCH",
          context: collectionMismatchDetails(
            this.name,
            expected.model,
            current.info.name,
          ),
        },
      );
    }

    if (expected.dimension !== current.info.dimension) {
      throw new EngineError(
        "Collection embedding dimension does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_DIMENSION_MISMATCH",
          context: collectionMismatchDetails(
            this.name,
            expected.dimension,
            current.info.dimension,
          ),
        },
      );
    }

    if (expected.metric !== current.info.metric) {
      throw new EngineError(
        "Collection embedding metric does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_METRIC_MISMATCH",
          context: collectionMismatchDetails(
            this.name,
            expected.metric,
            current.info.metric,
          ),
        },
      );
    }
  }

  private requireEmbeddingModel(operation: string): EmbeddingModel {
    if (!this.embeddingModel) {
      throw new EngineError(
        "Collection operation requires an embedding model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_MODEL_REQUIRED",
          context: collectionOperationDetails(this.name, operation),
        },
      );
    }

    return this.embeddingModel;
  }
}

function collectionOperationDetails(
  name: string,
  operation: string,
): string | undefined {
  return errorDetails([collectionDetail(name), detail("operation", operation)]);
}

function collectionMismatchDetails(
  name: string,
  expected: string | number,
  actual: string | number,
): string | undefined {
  return errorDetails([
    collectionDetail(name),
    detail("expected", expected),
    detail("actual", actual),
  ]);
}

export function isCollectionIndexed(
  info: CollectionInfo | null | undefined,
): info is CollectionInfo & {
  embedding: CollectionEmbeddingSchema;
  indexVersion: number;
} {
  return (
    info?.embedding !== null &&
    info?.embedding !== undefined &&
    info.indexVersion !== null &&
    info.indexVersion !== undefined
  );
}

function requireIndexedEmbedding(
  info: CollectionInfo,
  operation: string,
): CollectionEmbeddingSchema {
  if (isCollectionIndexed(info)) {
    return info.embedding;
  }

  throw new EngineError("Collection index has not been built", {
    code: "ZVEC_GREP.ENGINE.COLLECTION.INDEX_MISSING",
    context: errorDetails([
      collectionDetail(info.name),
      detail("operation", operation),
      detail("hint", "Run zg index to build this index."),
    ]),
  });
}

export class CollectionRegistry {
  private readonly meta: ZvecCollectionsMetaStore;
  private readonly collections = new Map<string, Collection>();
  private closed = false;

  constructor(
    readonly home: string = defaultHome(),
    private readonly embeddingModel?: EmbeddingModel,
    private readonly readOnly = false,
  ) {
    if (!readOnly) {
      mkdirSync(home, { recursive: true });
    }

    this.meta = new ZvecCollectionsMetaStore(
      collectionsMetaPath(home),
      readOnly,
    );
  }

  list(): CollectionInfo[] {
    return this.meta.list();
  }

  get(name: string): CollectionInfo | null {
    return this.meta.getByName(name);
  }

  has(name: string): boolean {
    return this.get(name) !== null;
  }

  remove(name: string): boolean {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot remove a collection from a read-only registry",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: collectionOperationDetails(name, "remove"),
        },
      );
    }

    const info = this.get(name);
    if (!info) {
      return false;
    }

    const cached = this.collections.get(name);
    if (cached) {
      cached.close();
      this.collections.delete(name);
    }

    this.meta.remove(info.id);
    const files = new ZvecFileMetaStore(filesMetaPath(this.home), false);
    try {
      files.deleteCollection(info.id);
    } finally {
      files.close();
    }
    rmSync(info.path, { recursive: true, force: true });

    return true;
  }

  rename(name: string, nextName: string): CollectionInfo | null {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot rename a collection from a read-only registry",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: collectionOperationDetails(name, "rename"),
        },
      );
    }

    const normalizedNextName = nextName.trim();
    if (normalizedNextName.length === 0) {
      throw new EngineError("Collection rename requires a non-empty name", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.EMPTY_NAME",
        context: collectionOperationDetails(name, "rename"),
      });
    }

    const info = this.get(name);
    if (!info) {
      return null;
    }

    if (this.has(normalizedNextName)) {
      throw new EngineError("Collection already exists", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.ALREADY_EXISTS",
        context: errorDetails([collectionDetail(normalizedNextName)]),
      });
    }

    const cached = this.collections.get(name);
    if (cached) {
      cached.close();
      this.collections.delete(name);
    }

    const updated: CollectionInfo = {
      ...info,
      name: normalizedNextName,
      updatedTime: Date.now(),
    };

    this.meta.upsert(updated);

    return updated;
  }

  async status(name: string): Promise<CollectionIndexStatus | null> {
    const info = this.get(name);
    if (
      !info ||
      !isCollectionIndexed(info) ||
      info.indexPolicy === "disabled"
    ) {
      return null;
    }

    return this.open(name).status();
  }

  create(
    name: string,
    rootPaths: readonly (string | RootPath)[],
    path?: string,
  ): CollectionInfo {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot create a collection in a read-only registry",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: collectionOperationDetails(name, "create"),
        },
      );
    }

    if (!this.embeddingModel) {
      throw new EngineError(
        "Creating a collection requires an embedding model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_MODEL_REQUIRED",
          context: collectionOperationDetails(name, "create"),
        },
      );
    }

    if (this.has(name)) {
      throw new EngineError("Collection already exists", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.ALREADY_EXISTS",
        context: errorDetails([collectionDetail(name)]),
      });
    }

    const id = randomUUID();
    const collectionPath = normalizePath(
      path ?? join(this.home, "collections", id),
    );
    const now = Date.now();
    const normalizedRootPaths = validateRootPaths(rootPaths);
    const info: CollectionInfo = {
      id,
      name,
      path: collectionPath,
      rootPaths: normalizedRootPaths,
      indexPolicy: "enabled",
      embedding: currentEmbeddingSchema(this.embeddingModel),
      indexVersion: CURRENT_INDEX_VERSION,
      createdTime: now,
      updatedTime: now,
    };

    mkdirSync(collectionPath, { recursive: true });
    this.meta.upsert(info);

    return info;
  }

  prepareIndex(
    name: string,
    rootPaths: readonly (string | RootPath)[],
    path?: string,
  ): CollectionInfo {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot prepare a collection in a read-only registry",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: collectionOperationDetails(name, "prepareIndex"),
        },
      );
    }

    if (!this.embeddingModel) {
      throw new EngineError(
        "Preparing a collection requires an embedding model",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.EMBEDDING_MODEL_REQUIRED",
          context: collectionOperationDetails(name, "prepareIndex"),
        },
      );
    }

    const existing = this.get(name);
    if (!existing) {
      return this.create(name, rootPaths, path);
    }

    const normalizedRootPaths = validateRootPaths(rootPaths);
    const collectionPath = normalizePath(path ?? existing.path);
    const cached = this.collections.get(name);
    if (cached) {
      cached.close();
      this.collections.delete(name);
    }

    const updated: CollectionInfo = {
      ...existing,
      path: collectionPath,
      rootPaths: normalizedRootPaths,
      indexPolicy: "enabled",
      embedding: currentEmbeddingSchema(this.embeddingModel),
      indexVersion: CURRENT_INDEX_VERSION,
      updatedTime: Date.now(),
    };

    mkdirSync(collectionPath, { recursive: true });
    this.meta.upsert(updated);

    return updated;
  }

  disableIndex(
    name: string,
    rootPaths: readonly (string | RootPath)[],
    path?: string,
  ): CollectionInfo {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot disable a collection index in a read-only registry",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: collectionOperationDetails(name, "disableIndex"),
        },
      );
    }

    const existing = this.get(name);
    const normalizedRootPaths = validateRootPaths(rootPaths);
    const now = Date.now();

    if (existing) {
      const cached = this.collections.get(name);
      if (cached) {
        cached.close();
        this.collections.delete(name);
      }

      const updated: CollectionInfo = {
        ...existing,
        rootPaths: normalizedRootPaths,
        path: normalizePath(path ?? existing.path),
        indexPolicy: "disabled",
        embedding: null,
        indexVersion: null,
        updatedTime: now,
      };

      this.meta.upsert(updated);
      return updated;
    }

    const collectionPath = normalizePath(
      path ?? join(this.home, "collections", randomUUID()),
    );
    const info: CollectionInfo = {
      id: randomUUID(),
      name,
      path: collectionPath,
      rootPaths: normalizedRootPaths,
      indexPolicy: "disabled",
      embedding: null,
      indexVersion: null,
      createdTime: now,
      updatedTime: now,
    };

    this.meta.upsert(info);
    return info;
  }

  updateRootPaths(
    name: string,
    rootPaths: readonly (string | RootPath)[],
  ): CollectionInfo | null {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot update a collection from a read-only registry",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: collectionOperationDetails(name, "updateRootPaths"),
        },
      );
    }

    const info = this.get(name);
    if (!info) {
      return null;
    }

    const normalizedRootPaths = validateRootPaths(rootPaths);
    if (rootPathsEqual(info.rootPaths, normalizedRootPaths)) {
      return info;
    }

    const cached = this.collections.get(name);
    if (cached) {
      cached.close();
      this.collections.delete(name);
    }

    const updated: CollectionInfo = {
      ...info,
      rootPaths: normalizedRootPaths,
      updatedTime: Date.now(),
    };

    this.meta.upsert(updated);
    return updated;
  }

  open(name: string): Collection {
    const cached = this.collections.get(name);
    if (cached) {
      return cached;
    }

    const info = this.get(name);
    if (!info) {
      throw new EngineError("Collection not found", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.NOT_FOUND",
        context: errorDetails([collectionDetail(name)]),
      });
    }

    if (info.indexPolicy === "disabled") {
      throw new EngineError("Collection index is disabled", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.INDEX_DISABLED",
        context: errorDetails([
          collectionDetail(name),
          detail(
            "hint",
            "Run zg index to enable and build this index, or use zg query --rg for no-index search.",
          ),
        ]),
      });
    }

    if (!isCollectionIndexed(info)) {
      throw new EngineError("Collection index has not been built", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.INDEX_MISSING",
        context: errorDetails([
          collectionDetail(name),
          detail("hint", "Run zg index to build this index."),
        ]),
      });
    }

    const collection = new Collection(
      info,
      this.embeddingModel,
      this.readOnly,
      filesMetaPath(this.home),
    );
    this.collections.set(name, collection);
    return collection;
  }

  close(): void {
    if (this.closed) {
      return;
    }

    for (const collection of this.collections.values()) {
      collection.close();
    }

    this.collections.clear();
    this.meta.close();
    this.closed = true;
  }
}

class ZvecCollectionsMetaStore {
  private readonly collection: ZVecCollection | null;
  private needsOptimize = false;

  constructor(
    private readonly path: string,
    private readonly readOnly = false,
  ) {
    initializeZvec();
    if (!readOnly) {
      mkdirSync(homeDirectoryForMetaPath(path), { recursive: true });
    }

    if (existsSync(path)) {
      this.collection = openZvecCollection(path, readOnly, "open", () =>
        ZVecOpen(path, { readOnly }),
      );
    } else if (readOnly) {
      this.collection = null;
    } else {
      this.collection = openZvecCollection(path, readOnly, "create", () =>
        ZVecCreateAndOpen(path, createCollectionsSchema()),
      );
    }
  }

  list(): CollectionInfo[] {
    if (!this.collection) {
      return [];
    }

    const topk = Math.max(this.collection.stats.docCount, 1);
    return this.collection
      .querySync({ topk, includeVector: false })
      .map((doc) => docToCollectionInfo(doc))
      .sort((left, right) => left.name.localeCompare(right.name));
  }

  getByName(name: string): CollectionInfo | null {
    if (!this.collection) {
      return null;
    }

    const [doc] = this.collection.querySync({
      filter: `name = ${quoteFilterString(name)}`,
      topk: 1,
      includeVector: false,
    });

    return doc ? docToCollectionInfo(doc) : null;
  }

  upsert(info: CollectionInfo): void {
    this.assertWritable("upsert");
    const collection = this.requireCollection();
    const deleteStatus = collection.deleteSync(info.id);
    assertZvecStatusOrNotFound(
      deleteStatus,
      "collection metadata replace",
      info.name,
    );
    const status = collection.upsertSync(collectionInfoToDoc(info));
    assertZvecStatus(status, "collection metadata upsert", info.name);
    this.needsOptimize = true;
  }

  remove(collectionId: string): void {
    this.assertWritable("remove");
    const status = this.requireCollection().deleteByFilterSync(
      `collection_id = ${quoteFilterString(collectionId)}`,
    );
    assertZvecStatus(status, "collection metadata delete", collectionId);
    this.needsOptimize = true;
  }

  close(): void {
    if (!this.collection) {
      return;
    }

    if (this.needsOptimize) {
      this.collection.optimizeSync();
      this.needsOptimize = false;
    }

    this.collection.closeSync();
  }

  private requireCollection(): ZVecCollection {
    if (!this.collection) {
      throw new EngineError("zvec collection metadata storage is unavailable", {
        code: "ZVEC_GREP.ENGINE.COLLECTION.META_UNAVAILABLE",
        context: `path=${this.path}`,
      });
    }

    return this.collection;
  }

  private assertWritable(operation: string): void {
    if (this.readOnly) {
      throw new EngineError(
        "Cannot update read-only collection metadata storage",
        {
          code: "ZVEC_GREP.ENGINE.COLLECTION.READ_ONLY",
          context: `path=${this.path} operation=${operation}`,
        },
      );
    }
  }
}

function assertZvecStatusOrNotFound(
  status: { ok: boolean; code: string; message: string },
  operation: string,
  context: string,
): void {
  if (status.ok || status.code === "ZVEC_NOT_FOUND") {
    return;
  }

  assertZvecStatus(status, operation, context);
}

function createCollectionsSchema(): ZVecCollectionSchema {
  return new ZVecCollectionSchema({
    name: "zvec_grep_collections",
    fields: [
      indexedStringField("collection_id"),
      indexedStringField("name"),
      indexedStringField("path"),
      stringField("root_paths_json"),
      indexedStringField("index_policy", true),
      indexedStringField("embedding_provider", true),
      indexedStringField("embedding_model", true),
      {
        name: "embedding_dimension",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      indexedStringField("embedding_metric", true),
      {
        name: "index_version",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      {
        name: "created_time",
        dataType: ZVecDataType.INT64,
        nullable: false,
      },
      {
        name: "updated_time",
        dataType: ZVecDataType.INT64,
        nullable: false,
      },
    ],
  });
}

function collectionInfoToDoc(info: CollectionInfo): ZVecDocInput {
  const fields: Record<string, string | number> = {
    collection_id: info.id,
    name: info.name,
    path: info.path,
    root_paths_json: JSON.stringify(info.rootPaths),
    created_time: info.createdTime,
    updated_time: info.updatedTime,
  };

  if (info.indexPolicy !== undefined) {
    fields.index_policy = info.indexPolicy;
  }

  if (info.embedding) {
    fields.embedding_provider = info.embedding.provider;
    fields.embedding_model = info.embedding.model;
    fields.embedding_dimension = info.embedding.dimension;
    fields.embedding_metric = info.embedding.metric;
  }

  if (info.indexVersion !== null && info.indexVersion !== undefined) {
    fields.index_version = info.indexVersion;
  }

  return {
    id: info.id,
    fields,
  };
}

function docToCollectionInfo(doc: ZVecDoc): CollectionInfo {
  const fields = doc.fields;
  const embedding = collectionEmbeddingFromFields(fields);

  return {
    id: readStringFieldFromFields(fields, "collection_id"),
    name: readStringFieldFromFields(fields, "name"),
    path: normalizePath(readStringFieldFromFields(fields, "path")),
    rootPaths: parseRootPaths(
      readStringFieldFromFields(fields, "root_paths_json"),
    ),
    indexPolicy: collectionIndexPolicyFromFields(fields),
    embedding,
    indexVersion: readNullableNumberFieldFromFields(fields, "index_version"),
    createdTime: readNumberFieldFromFields(fields, "created_time"),
    updatedTime: readNumberFieldFromFields(fields, "updated_time"),
  };
}

function collectionEmbeddingFromFields(
  fields: Record<string, unknown>,
): CollectionEmbeddingSchema | null {
  const provider = readNullableStringFieldFromFields(
    fields,
    "embedding_provider",
  );
  const model = readNullableStringFieldFromFields(fields, "embedding_model");
  const dimension = readNullableNumberFieldFromFields(
    fields,
    "embedding_dimension",
  );
  const metric = readNullableStringFieldFromFields(fields, "embedding_metric");

  if (!provider || !model || !dimension || !metric) {
    return null;
  }

  return {
    provider,
    model,
    dimension,
    metric,
  };
}

function collectionIndexPolicyFromFields(
  fields: Record<string, unknown>,
): CollectionIndexPolicy | undefined {
  const policy = readNullableStringFieldFromFields(fields, "index_policy");
  return policy === "enabled" || policy === "disabled" ? policy : undefined;
}

function parseRootPaths(value: string): RootPath[] {
  try {
    const parsed = JSON.parse(value) as unknown;
    return Array.isArray(parsed) ? validateRootPaths(parsed as RootPath[]) : [];
  } catch {
    return [];
  }
}

function collectionsMetaPath(home: string): string {
  return join(home, COLLECTIONS_ZVEC);
}

function filesMetaPath(home: string): string {
  return join(home, FILES_ZVEC);
}

function homeDirectoryForMetaPath(path: string): string {
  return dirname(path);
}

function indexedStringField(name: string, nullable = false) {
  return {
    name,
    dataType: ZVecDataType.STRING,
    nullable,
    indexParams: { indexType: ZVecIndexType.INVERT },
  };
}

function stringField(name: string, nullable = false) {
  return {
    name,
    dataType: ZVecDataType.STRING,
    nullable,
  };
}

export function defaultCollectionName(path: string): string {
  return basename(normalizePath(path));
}

function currentEmbeddingSchema(
  model: EmbeddingModel,
): CollectionEmbeddingSchema {
  return {
    provider: model.info.provider,
    model: model.info.name,
    dimension: model.info.dimension,
    metric: model.info.metric,
  };
}

function rootPathsEqual(
  left: readonly RootPath[],
  right: readonly RootPath[],
): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}
