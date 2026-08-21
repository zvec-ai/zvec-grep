import {
  ZVecCollectionSchema,
  ZVecCreateAndOpen,
  ZVecDataType,
  ZVecIndexType,
  ZVecInitialize,
  ZVecLogLevel,
  ZVecMetricType,
  ZVecOpen,
  type ZVecCollection,
  type ZVecDoc,
  type ZVecDocInput,
  type ZVecStatus,
} from "@zvec/zvec";
import { closeSync, existsSync, mkdirSync, openSync } from "node:fs";
import { dirname, join } from "node:path";
import { EngineError } from "../errors.js";
import type {
  CodeEntityModifier,
  CodeSymbolType,
  WorkspaceIndexEmbeddingSchema,
  Content,
  Entity,
  EntityFragment,
  EntityMetadata,
  FileInfo,
  ImageFormat,
  Range,
} from "../types.js";
import { normalizePath } from "../utils/path.js";
import type {
  WorkspaceIndexStorage,
  WorkspaceIndexStorageOptions,
} from "./index.js";
import { resolveWorkspaceIndexStoragePaths } from "./layout.js";

type FileRecord = FileInfo & {
  entityIds: string[];
};

type StoredEntity = {
  entity: Entity;
  file: FileInfo;
};

type IndexedFragment = {
  fragment: EntityFragment;
  vector: readonly number[];
};

type FileIndexDiagnostics = {
  truncatedFragmentCount?: number;
};

type StorageSearchFilter = {
  fileIds?: readonly string[];
  groupIds?: readonly string[];
  symbolNames?: readonly string[];
  symbolTypes?: readonly CodeSymbolType[];
};

type StorageSearchHit = {
  fragment: EntityFragment;
  file: FileInfo;
  path: "fts" | "vector";
  score: number;
};

const ENTITY_VECTOR_FIELD = "embedding";
const ENTITY_TEXT_FIELD = "text";
const ZVEC_LOCK_FILE = "LOCK";
const NO_MATCH_FILTER = "file_id = '__zvec_grep_no_match__'";
const ZVEC_UPSERT_BATCH_SIZE = 1024;
const ZVEC_MAX_QUERY_TOPK = 100_000;
const FILE_ID_HEX_ALPHABET = "0123456789abcdef";
const ZVEC_OPEN_RETRY_ATTEMPTS = 8;
const ZVEC_OPEN_RETRY_BASE_DELAY_MS = 100;
const ZVEC_OPEN_RETRY_MAX_DELAY_MS = 1000;

let zvecInitialized = false;

export function createWorkspaceIndexStorage(
  options: WorkspaceIndexStorageOptions,
): WorkspaceIndexStorage {
  return new ZvecWorkspaceIndexStorage(options);
}

type StoredEntityFragment = {
  fragment: EntityFragment;
  file: FileInfo;
};

class ZvecWorkspaceIndexStorage implements WorkspaceIndexStorage {
  readonly readOnly: boolean;
  private readonly collection: ZVecCollection;
  private readonly files: ZvecFileMetaStore;
  private readonly filesById = new Map<string, FileRecord>();
  private readonly filesByAbsolutePath = new Map<string, FileRecord>();
  private needsOptimize = false;

  constructor(options: WorkspaceIndexStorageOptions) {
    const paths = resolveWorkspaceIndexStoragePaths(options.storagePath);
    const { readOnly } = options;
    this.readOnly = readOnly;
    initializeZvec();
    if (!readOnly) {
      mkdirSync(paths.storagePath, { recursive: true });
    }

    this.files = new ZvecFileMetaStore(paths.filesPath, readOnly);
    for (const file of this.files.list()) {
      this.rememberFile(file);
    }

    const zvecPath = paths.indexPath;
    if (existsSync(zvecPath)) {
      this.collection = openZvecCollection(zvecPath, readOnly, "open", () =>
        ZVecOpen(zvecPath, { readOnly }),
      );
    } else if (readOnly) {
      throw new EngineError("zvec collection storage does not exist", {
        code: "ZVEC_GREP.ENGINE.STORAGE.ZVEC_COLLECTION_MISSING",
        context: `path=${zvecPath}`,
      });
    } else {
      this.collection = openZvecCollection(zvecPath, readOnly, "create", () =>
        ZVecCreateAndOpen(zvecPath, createSchema(options.embedding)),
      );
    }
  }

  getFileByPath(absolutePath: string): FileInfo | null {
    const file = this.filesByAbsolutePath.get(normalizePath(absolutePath));

    return file ? fileRecordToInfo(file) : null;
  }

  listFilesByPathPrefix(absolutePath: string): FileInfo[] {
    return this.listFilesByPathPrefixes([absolutePath]);
  }

  listFilesByPathPrefixes(absolutePaths: readonly string[]): FileInfo[] {
    const prefixes = new Set(absolutePaths.map((path) => normalizePath(path)));
    if (prefixes.size === 0) {
      return [];
    }
    return [...this.filesByAbsolutePath.entries()]
      .filter(([path]) => hasPathPrefix(path, prefixes))
      .map(([, file]) => fileRecordToInfo(file));
  }

  listFiles(): FileInfo[] {
    return [...this.filesById.values()].map(fileRecordToInfo);
  }

  listEntitiesByFile(
    fileId: string,
    options: { limit?: number; offset?: number } = {},
  ): StoredEntity[] {
    const file = this.filesById.get(fileId);
    if (!file) {
      return [];
    }

    const offset = options.offset ?? 0;
    const limit = options.limit ?? file.entityIds.length;
    const ids = file.entityIds.slice(offset, offset + limit);

    return this.fetchStoredEntities(ids);
  }

  getEntity(entityId: string): StoredEntity | null {
    const fragment = this.getFragment(entityId);

    if (!fragment) {
      return null;
    }

    if (
      fragment.fragment.group &&
      fragment.fragment.group !== fragment.fragment.id
    ) {
      return this.getEntity(fragment.fragment.group);
    }

    return {
      file: fragment.file,
      entity: fragmentToEntity(fragment.fragment),
    };
  }

  private getFragment(fragmentId: string): StoredEntityFragment | null {
    const docs = this.collection.fetchSync({
      ids: fragmentId,
      includeVector: false,
    });
    const doc = docs[fragmentId];

    if (!doc) {
      return null;
    }

    return this.docToStoredFragment(doc);
  }

  replaceFile(
    file: FileInfo,
    entries: readonly IndexedFragment[],
    diagnostics: FileIndexDiagnostics = {},
  ): void {
    this.assertWritable("replaceFile");
    const fragments = entries.map(({ fragment }) => fragment);
    validateFragmentGroups(file.id, fragments);
    this.markFileDirty(file);
    this.deleteFileDocuments(file.id);

    const now = Date.now();
    const entityIds = publicEntityIds(fragments);
    const indexedFile: FileRecord = {
      ...file,
      absolutePath: normalizePath(file.absolutePath),
      indexStatus: {
        indexedTime: now,
        entityCount: entityIds.length,
        truncatedFragmentCount: diagnostics.truncatedFragmentCount ?? 0,
      },
      entityIds,
    };

    if (entries.length > 0) {
      const docs = entries.map(({ fragment, vector }, index): ZVecDocInput => ({
        id: fragment.id,
        vectors: {
          [ENTITY_VECTOR_FIELD]: [...vector],
        },
        fields: fragmentToFields(indexedFile, fragment, index),
      }));

      this.upsertDocs(file.id, docs);

      this.needsOptimize = true;
    }

    this.rememberFile(indexedFile);
    this.persistFile(indexedFile);
  }

  private upsertDocs(fileId: string, docs: readonly ZVecDocInput[]): void {
    for (let start = 0; start < docs.length; start += ZVEC_UPSERT_BATCH_SIZE) {
      const batch = docs.slice(start, start + ZVEC_UPSERT_BATCH_SIZE);
      const statuses = this.collection.upsertSync(batch);
      const failed = statuses.find((status) => !status.ok);

      if (failed) {
        throw new EngineError("zvec failed to upsert entity documents", {
          code: "ZVEC_GREP.ENGINE.STORAGE.ZVEC_UPSERT_FAILED",
          context: `fileId=${fileId} batchStart=${start} batchSize=${batch.length} code=${failed.code} message=${failed.message}`,
        });
      }
    }
  }

  markFileFailed(file: FileInfo, error: string): void {
    this.assertWritable("markFileFailed");
    this.deleteFileDocuments(file.id);

    this.rememberFile({
      ...file,
      absolutePath: normalizePath(file.absolutePath),
      indexStatus: {
        indexedTime: null,
        entityCount: 0,
        error,
      },
      entityIds: [],
    });
    this.persistFile(this.filesById.get(file.id)!);
  }

  deleteFile(fileId: string): void {
    this.assertWritable("deleteFile");
    const existing = this.filesById.get(fileId);

    this.deleteFileDocuments(fileId);

    if (existing) {
      this.filesById.delete(fileId);
      this.filesByAbsolutePath.delete(normalizePath(existing.absolutePath));
    }
    this.files.deleteFile(fileId);
  }

  searchFts(
    query: string,
    limit: number,
    filter?: StorageSearchFilter,
  ): StorageSearchHit[] {
    const zvecFilter = buildFilter(filter);
    const docs = this.collection.querySync({
      fieldName: ENTITY_TEXT_FIELD,
      fts: { matchString: query },
      ...(zvecFilter ? { filter: zvecFilter } : {}),
      topk: limit,
      includeVector: false,
    });

    return docsToHits(docs, "fts", this);
  }

  searchVector(
    vector: readonly number[],
    limit: number,
    filter?: StorageSearchFilter,
  ): StorageSearchHit[] {
    const zvecFilter = buildFilter(filter);
    const docs = this.collection.querySync({
      fieldName: ENTITY_VECTOR_FIELD,
      vector: [...vector],
      ...(zvecFilter ? { filter: zvecFilter } : {}),
      topk: limit,
      includeVector: false,
    });

    return docsToHits(docs, "vector", this);
  }

  async finalizeWrites(): Promise<void> {
    this.assertWritable("finalizeWrites");
    if (this.needsOptimize) {
      await this.collection.optimize();
      this.needsOptimize = false;
    }
  }

  close(): void {
    if (this.needsOptimize) {
      this.collection.optimizeSync();
      this.needsOptimize = false;
    }

    this.files.close();
    this.collection.closeSync();
  }

  private fetchStoredEntities(ids: readonly string[]): StoredEntity[] {
    if (ids.length === 0) {
      return [];
    }

    const docs = this.collection.fetchSync({
      ids: [...ids],
      includeVector: false,
    });
    const entities: StoredEntity[] = [];

    for (const id of ids) {
      const doc = docs[id];
      const entity = doc ? this.docToStoredEntity(doc) : null;

      if (entity) {
        entities.push(entity);
      }
    }

    return entities;
  }

  docToStoredEntity(doc: ZVecDoc): StoredEntity | null {
    const stored = this.docToStoredFragment(doc);

    if (!stored) {
      return null;
    }

    if (stored.fragment.group && stored.fragment.group !== stored.fragment.id) {
      return this.getEntity(stored.fragment.group);
    }

    return {
      file: stored.file,
      entity: fragmentToEntity(stored.fragment),
    };
  }

  docToStoredFragment(doc: ZVecDoc): StoredEntityFragment | null {
    const fileId = readStringField(doc, "file_id");
    const file = this.filesById.get(fileId);

    if (!file) {
      return null;
    }

    return {
      file: fileRecordToInfo(file),
      fragment: {
        id: doc.id,
        group:
          readNullableStringFieldFromFields(doc.fields, "group") ?? undefined,
        fileId,
        range: parseRange(readStringField(doc, "range_json")),
        content: parseContent(doc.fields),
        metadata: parseMetadata(doc.fields),
      },
    };
  }

  private rememberFile(file: FileRecord): void {
    const normalized = {
      ...file,
      absolutePath: normalizePath(file.absolutePath),
    };

    this.filesById.set(normalized.id, normalized);
    this.filesByAbsolutePath.set(normalized.absolutePath, normalized);
  }

  private markFileDirty(file: FileInfo): void {
    this.rememberFile({
      ...file,
      absolutePath: normalizePath(file.absolutePath),
      indexStatus: {
        indexedTime: null,
        entityCount: 0,
      },
      entityIds: [],
    });
    this.persistFile(this.filesById.get(file.id)!);
  }

  private deleteFileDocuments(fileId: string): void {
    this.collection.deleteByFilterSync(
      `file_id = ${quoteFilterString(fileId)}`,
    );
    this.needsOptimize = true;
  }

  private persistFile(file: FileRecord): void {
    this.files.upsertFile(file);
  }

  private assertWritable(operation: string): void {
    if (this.readOnly) {
      throw new EngineError("Cannot update read-only workspace index storage", {
        code: "ZVEC_GREP.ENGINE.STORAGE.READ_ONLY",
        context: `operation=${operation}`,
      });
    }
  }
}

function hasPathPrefix(path: string, prefixes: ReadonlySet<string>): boolean {
  let current = path;
  while (true) {
    if (prefixes.has(current)) {
      return true;
    }
    const parent = dirname(current);
    if (parent === current) {
      return false;
    }
    current = parent;
  }
}

class ZvecFileMetaStore {
  private readonly collection: ZVecCollection;
  private needsOptimize = false;

  constructor(
    private readonly path: string,
    private readonly readOnly = false,
  ) {
    initializeZvec();
    if (!readOnly) {
      mkdirSync(dirname(path), { recursive: true });
    }

    if (existsSync(path)) {
      this.collection = openZvecCollection(path, readOnly, "open", () =>
        ZVecOpen(path, { readOnly }),
      );
    } else if (readOnly) {
      throw new EngineError("zvec file metadata storage does not exist", {
        code: "ZVEC_GREP.ENGINE.STORAGE.ZVEC_FILE_META_MISSING",
        context: `path=${path}`,
      });
    } else {
      this.collection = openZvecCollection(path, readOnly, "create", () =>
        ZVecCreateAndOpen(path, createFilesSchema()),
      );
    }
  }

  list(): FileRecord[] {
    const docs = queryFileMetadataDocs(this.collection, ZVEC_MAX_QUERY_TOPK);

    return docs
      .map((doc) => docToFileRecord(doc))
      .sort((left, right) =>
        left.relativePath.localeCompare(right.relativePath),
      );
  }

  upsertFile(file: FileRecord): void {
    this.assertWritable("upsertFile");
    const deleteStatus = this.collection.deleteSync(file.id);
    assertZvecStatusOrNotFound(deleteStatus, "file metadata replace", file.id);
    const status = this.collection.upsertSync(fileRecordToDoc(file));
    assertZvecStatus(status, "file metadata upsert", file.id);
    this.needsOptimize = true;
  }

  deleteFile(fileId: string): void {
    this.assertWritable("deleteFile");
    const status = this.collection.deleteByFilterSync(
      `file_id = ${quoteFilterString(fileId)}`,
    );
    assertZvecStatus(status, "file metadata delete", fileId);
    this.needsOptimize = true;
  }

  close(): void {
    if (this.needsOptimize) {
      this.collection.optimizeSync();
      this.needsOptimize = false;
    }

    this.collection.closeSync();
  }

  private assertWritable(operation: string): void {
    if (this.readOnly) {
      throw new EngineError("Cannot update read-only file metadata storage", {
        code: "ZVEC_GREP.ENGINE.STORAGE.FILE_META_READ_ONLY",
        context: `path=${this.path} operation=${operation}`,
      });
    }
  }
}

export function queryFileMetadataDocs(
  collection: Pick<ZVecCollection, "querySync" | "stats">,
  maxTopk = ZVEC_MAX_QUERY_TOPK,
): ZVecDoc[] {
  if (!Number.isInteger(maxTopk) || maxTopk < 1) {
    throw new EngineError("zvec metadata query limit must be positive", {
      code: "ZVEC_GREP.ENGINE.STORAGE.INVALID_METADATA_QUERY_LIMIT",
      context: `maxTopk=${maxTopk}`,
    });
  }

  if (collection.stats.docCount <= maxTopk) {
    return collection.querySync({
      topk: Math.max(collection.stats.docCount, 1),
      includeVector: false,
    });
  }

  return FILE_ID_HEX_ALPHABET.split("").flatMap((prefix) =>
    queryFileMetadataPartition(collection, prefix, maxTopk),
  );
}

function queryFileMetadataPartition(
  collection: Pick<ZVecCollection, "querySync">,
  prefix: string,
  maxTopk: number,
): ZVecDoc[] {
  const lower = quoteFilterString(prefix);
  const upper = quoteFilterString(`${prefix}g`);
  const docs = collection.querySync({
    filter: `file_id >= ${lower} AND file_id < ${upper}`,
    topk: maxTopk,
    includeVector: false,
  });

  if (docs.length < maxTopk) {
    return docs;
  }

  if (prefix.length >= 64) {
    throw new EngineError("zvec metadata partition exceeds query limit", {
      code: "ZVEC_GREP.ENGINE.STORAGE.METADATA_PARTITION_TOO_LARGE",
      context: `prefix=${prefix} maxTopk=${maxTopk}`,
    });
  }

  return FILE_ID_HEX_ALPHABET.split("").flatMap((suffix) =>
    queryFileMetadataPartition(collection, `${prefix}${suffix}`, maxTopk),
  );
}

function createFilesSchema(): ZVecCollectionSchema {
  return new ZVecCollectionSchema({
    name: "zvec_grep_files",
    fields: [
      indexedStringField("file_id"),
      indexedStringField("absolute_path"),
      stringField("relative_path"),
      stringField("root_path"),
      {
        name: "size_bytes",
        dataType: ZVecDataType.INT64,
        nullable: false,
      },
      {
        name: "last_modified_time",
        dataType: ZVecDataType.INT64,
        nullable: false,
      },
      indexedStringField("content_hash", true),
      indexedStringField("kind"),
      indexedStringField("format"),
      {
        name: "has_index_status",
        dataType: ZVecDataType.BOOL,
        nullable: false,
      },
      {
        name: "indexed_time",
        dataType: ZVecDataType.INT64,
        nullable: true,
      },
      {
        name: "entity_count",
        dataType: ZVecDataType.INT32,
        nullable: false,
      },
      {
        name: "token_count",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      {
        name: "truncated_fragment_count",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      stringField("error", true),
      stringField("entity_ids_json"),
    ],
  });
}

function fileRecordToDoc(file: FileRecord): ZVecDocInput {
  const fields: Record<string, string | number | boolean> = {
    file_id: file.id,
    absolute_path: normalizePath(file.absolutePath),
    relative_path: file.relativePath,
    root_path: file.rootPath,
    size_bytes: file.sizeBytes,
    last_modified_time: file.lastModifiedTime,
    kind: file.kind,
    format: file.format,
    has_index_status: file.indexStatus !== undefined,
    entity_count: file.indexStatus?.entityCount ?? 0,
    entity_ids_json: JSON.stringify(file.entityIds),
  };

  if (file.contentHash !== undefined) {
    fields.content_hash = file.contentHash;
  }

  if (
    file.indexStatus?.indexedTime !== null &&
    file.indexStatus?.indexedTime !== undefined
  ) {
    fields.indexed_time = file.indexStatus.indexedTime;
  }

  if (file.indexStatus?.tokenCount !== undefined) {
    fields.token_count = file.indexStatus.tokenCount;
  }

  if (file.indexStatus?.truncatedFragmentCount !== undefined) {
    fields.truncated_fragment_count = file.indexStatus.truncatedFragmentCount;
  }

  if (file.indexStatus?.error !== undefined) {
    fields.error = file.indexStatus.error;
  }

  return {
    id: file.id,
    fields,
  };
}

function docToFileRecord(doc: ZVecDoc): FileRecord {
  const fields = doc.fields;
  const hasIndexStatus = readBooleanFieldFromFields(fields, "has_index_status");
  const indexedTime = readNullableNumberFieldFromFields(fields, "indexed_time");
  const tokenCount = readNullableNumberFieldFromFields(fields, "token_count");
  const truncatedFragmentCount = readNullableNumberFieldFromFields(
    fields,
    "truncated_fragment_count",
  );
  const error = readNullableStringFieldFromFields(fields, "error");

  return {
    id: readStringField(doc, "file_id"),
    absolutePath: normalizePath(readStringField(doc, "absolute_path")),
    relativePath: readStringField(doc, "relative_path"),
    rootPath: readStringField(doc, "root_path"),
    sizeBytes: readNumberFieldFromFields(fields, "size_bytes"),
    lastModifiedTime: readNumberFieldFromFields(fields, "last_modified_time"),
    contentHash:
      readNullableStringFieldFromFields(fields, "content_hash") ?? undefined,
    kind: readStringField(doc, "kind") as FileInfo["kind"],
    format: readStringField(doc, "format"),
    indexStatus: hasIndexStatus
      ? {
          indexedTime,
          entityCount: readNumberFieldFromFields(fields, "entity_count"),
          ...(tokenCount === null ? {} : { tokenCount }),
          ...(truncatedFragmentCount === null
            ? {}
            : { truncatedFragmentCount }),
          ...(error === null ? {} : { error }),
        }
      : undefined,
    entityIds: parseStringArray(readStringField(doc, "entity_ids_json")),
  };
}

function fileRecordToInfo(file: FileRecord): FileInfo {
  const { entityIds: _entityIds, ...info } = file;

  return info;
}

function fragmentToEntity(fragment: EntityFragment): Entity {
  return {
    id: publicEntityId(fragment),
    fileId: fragment.fileId,
    range: fragment.range,
    content: fragment.content,
    metadata: fragment.metadata,
  };
}

function publicEntityId(fragment: EntityFragment): string {
  return fragment.group ?? fragment.id;
}

function publicEntityIds(fragments: readonly EntityFragment[]): string[] {
  const ids = new Set<string>();

  for (const fragment of fragments) {
    if (!fragment.group || fragment.group === fragment.id) {
      ids.add(publicEntityId(fragment));
    }
  }

  return [...ids];
}

function validateFragmentGroups(
  fileId: string,
  fragments: readonly EntityFragment[],
): void {
  const ids = new Set<string>();
  const groups = new Map<string, EntityFragment[]>();

  for (const fragment of fragments) {
    if (fragment.fileId !== fileId) {
      throw new EngineError("Entity fragment belongs to the wrong file", {
        code: "ZVEC_GREP.ENGINE.STORAGE.FRAGMENT_FILE_MISMATCH",
        context: `fileId=${fileId} fragmentId=${fragment.id} fragmentFileId=${fragment.fileId}`,
      });
    }

    if (ids.has(fragment.id)) {
      throw new EngineError("Duplicate entity fragment id", {
        code: "ZVEC_GREP.ENGINE.STORAGE.DUPLICATE_FRAGMENT_ID",
        context: `fileId=${fileId} fragmentId=${fragment.id}`,
      });
    }

    ids.add(fragment.id);

    if (fragment.group) {
      const group = groups.get(fragment.group) ?? [];
      group.push(fragment);
      groups.set(fragment.group, group);
    }
  }

  for (const [groupId, group] of groups) {
    const majorCount = group.filter(
      (fragment) => fragment.id === groupId,
    ).length;

    if (majorCount !== 1) {
      throw new EngineError(
        "Fragment group must have exactly one major fragment",
        {
          code: "ZVEC_GREP.ENGINE.STORAGE.INVALID_FRAGMENT_GROUP",
          context: `fileId=${fileId} group=${groupId} majorCount=${majorCount}`,
        },
      );
    }
  }
}

function initializeZvec(): void {
  if (zvecInitialized) {
    return;
  }

  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  zvecInitialized = true;
}

function openZvecCollection(
  zvecPath: string,
  readOnly: boolean,
  action: "open" | "create",
  open: () => ZVecCollection,
): ZVecCollection {
  const lockWritable = canTouchZvecLock(zvecPath);
  let lastError: unknown;

  for (let attempt = 0; attempt < ZVEC_OPEN_RETRY_ATTEMPTS; attempt++) {
    try {
      return open();
    } catch (error) {
      lastError = error;

      if (
        attempt + 1 >= ZVEC_OPEN_RETRY_ATTEMPTS ||
        !isRetryableZvecOpenError(error, lockWritable)
      ) {
        break;
      }

      sleepSync(zvecOpenRetryDelayMs(attempt));
    }
  }

  throw new EngineError("Failed to open zvec collection storage", {
    code: "ZVEC_GREP.ENGINE.STORAGE.ZVEC_OPEN_FAILED",
    context: `path=${zvecPath} action=${action} readOnly=${readOnly} attempts=${ZVEC_OPEN_RETRY_ATTEMPTS}`,
    cause: lastError,
  });
}

function canTouchZvecLock(zvecPath: string): boolean {
  const lockPath = join(zvecPath, ZVEC_LOCK_FILE);
  if (!existsSync(lockPath)) {
    return true;
  }

  try {
    const fd = openSync(lockPath, "a");
    closeSync(fd);
    return true;
  } catch {
    return false;
  }
}

function isRetryableZvecOpenError(
  error: unknown,
  lockWritable: boolean,
): boolean {
  if (!lockWritable) {
    return false;
  }

  const message = error instanceof Error ? error.message : String(error);

  if (!/lock/i.test(message)) {
    return false;
  }

  return !/permission|access denied|eacces|eperm|read-only/i.test(message);
}

function zvecOpenRetryDelayMs(attempt: number): number {
  const exponential = ZVEC_OPEN_RETRY_BASE_DELAY_MS * 2 ** attempt;
  return Math.min(exponential, ZVEC_OPEN_RETRY_MAX_DELAY_MS);
}

function sleepSync(ms: number): void {
  const buffer = new SharedArrayBuffer(4);
  const values = new Int32Array(buffer);
  Atomics.wait(values, 0, 0, ms);
}

function createSchema(
  embedding: WorkspaceIndexEmbeddingSchema,
): ZVecCollectionSchema {
  return new ZVecCollectionSchema({
    name: "zvec_grep_entities",
    fields: [
      indexedStringField("group", true),
      indexedStringField("file_id"),
      stringField("content_kind"),
      stringField("content_hash", true),
      stringField("metadata_kind", true),
      indexedStringField("symbol_type", true),
      indexedStringField("symbol_name", true),
      stringField("symbol_scope", true),
      stringField("symbol_signature", true),
      {
        name: "symbol_arity",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      stringField("symbol_doc", true),
      stringField("symbol_modifiers", true),
      stringField("node_type", true),
      stringField("heading", true),
      {
        name: "heading_level",
        dataType: ZVecDataType.INT32,
        nullable: true,
      },
      {
        name: ENTITY_TEXT_FIELD,
        dataType: ZVecDataType.STRING,
        nullable: false,
        indexParams: {
          indexType: ZVecIndexType.FTS,
          tokenizerName: "jieba",
          filters: ["lowercase"],
        },
      },
      {
        name: "fragment_index",
        dataType: ZVecDataType.INT32,
        nullable: false,
      },
      { name: "range_json", dataType: ZVecDataType.STRING, nullable: false },
      { name: "content_base64", dataType: ZVecDataType.STRING, nullable: true },
      { name: "image_format", dataType: ZVecDataType.STRING, nullable: true },
    ],
    vectors: {
      name: ENTITY_VECTOR_FIELD,
      dataType: ZVecDataType.VECTOR_FP32,
      dimension: embedding.dimension,
      indexParams: {
        indexType: ZVecIndexType.HNSW,
        metricType: metricToZvec(embedding.metric),
      },
    },
  });
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

function metricToZvec(metric: string): ZVecMetricType {
  if (metric === "cosine") {
    return ZVecMetricType.COSINE;
  }

  if (metric === "dot") {
    return ZVecMetricType.IP;
  }

  if (metric === "euclidean") {
    return ZVecMetricType.L2;
  }

  throw new EngineError("Unsupported vector metric for zvec storage", {
    code: "ZVEC_GREP.ENGINE.STORAGE.UNSUPPORTED_VECTOR_METRIC",
    context: `metric=${metric}`,
  });
}

function fragmentToFields(
  file: FileRecord,
  fragment: EntityFragment,
  fragmentIndex: number,
): Record<string, unknown> {
  const contentFields = contentToFields(fragment.content);

  return {
    ...optionalFields({
      group: fragment.group,
      content_hash: file.contentHash,
    }),
    file_id: file.id,
    content_kind: fragment.content.kind,
    ...metadataToFields(fragment.metadata),
    fragment_index: fragmentIndex,
    range_json: JSON.stringify(fragment.range),
    ...contentFields,
  };
}

function contentToFields(content: Content): Record<string, unknown> {
  if (content.kind === "text") {
    return {
      text: content.text,
    };
  }

  return {
    text: `[image:${content.format}]`,
    content_base64: Buffer.from(content.data).toString("base64"),
    image_format: content.format,
  };
}

function metadataToFields(
  metadata: EntityMetadata | undefined,
): Record<string, unknown> {
  if (!metadata) {
    return {};
  }

  if (metadata.kind === "code") {
    return {
      metadata_kind: "code",
      symbol_type: metadata.symbolType,
      ...optionalFields({
        symbol_name: metadata.symbolName,
        symbol_scope: metadata.scope,
        symbol_signature: metadata.signature,
        symbol_arity: metadata.arity,
        symbol_doc: metadata.doc,
        symbol_modifiers:
          metadata.modifiers.length > 0 ? metadata.modifiers.join(" ") : null,
        node_type: metadata.nodeType,
      }),
    };
  }

  return {
    metadata_kind: "markdown",
    ...optionalFields({
      symbol_scope: metadata.scope,
      heading: metadata.heading,
      heading_level: metadata.level,
    }),
  };
}

function optionalFields(
  fields: Record<string, string | number | null | undefined>,
): Record<string, string | number> {
  const result: Record<string, string | number> = {};

  for (const [key, value] of Object.entries(fields)) {
    if (value !== null && value !== undefined) {
      result[key] = value;
    }
  }

  return result;
}

function parseContent(fields: Record<string, unknown>): Content {
  const kind = fields.content_kind;

  if (kind === "text") {
    return {
      kind: "text",
      text: readStringFieldFromFields(fields, "text"),
    };
  }

  if (kind === "image") {
    return {
      kind: "image",
      data: Buffer.from(
        readStringFieldFromFields(fields, "content_base64"),
        "base64",
      ),
      format: readStringFieldFromFields(fields, "image_format") as ImageFormat,
    };
  }

  throw new EngineError("Stored entity has unsupported content kind", {
    code: "ZVEC_GREP.ENGINE.STORAGE.UNSUPPORTED_STORED_CONTENT_KIND",
    context: `contentKind=${String(kind)}`,
  });
}

function parseMetadata(
  fields: Record<string, unknown>,
): EntityMetadata | undefined {
  const kind = fields.metadata_kind;

  if (kind === "code") {
    return {
      kind: "code",
      symbolType: readStringFieldFromFields(
        fields,
        "symbol_type",
      ) as CodeSymbolType,
      symbolName: readNullableStringFieldFromFields(fields, "symbol_name"),
      scope: readNullableStringFieldFromFields(fields, "symbol_scope"),
      nodeType: readNullableStringFieldFromFields(fields, "node_type"),
      signature: readNullableStringFieldFromFields(fields, "symbol_signature"),
      arity: readNullableNumberFieldFromFields(fields, "symbol_arity"),
      doc: readNullableStringFieldFromFields(fields, "symbol_doc"),
      modifiers: readCodeModifiers(
        readNullableStringFieldFromFields(fields, "symbol_modifiers"),
      ),
    };
  }

  if (kind === "markdown") {
    return {
      kind: "markdown",
      heading: readNullableStringFieldFromFields(fields, "heading"),
      level: readNullableNumberFieldFromFields(fields, "heading_level"),
      scope: readNullableStringFieldFromFields(fields, "symbol_scope"),
    };
  }

  return undefined;
}

function readCodeModifiers(value: string | null): CodeEntityModifier[] {
  if (!value) {
    return [];
  }

  return value
    .split(/\s+/)
    .filter(
      (item): item is CodeEntityModifier =>
        item === "exported" ||
        item === "async" ||
        item === "abstract" ||
        item === "static" ||
        item === "public" ||
        item === "private" ||
        item === "protected" ||
        item === "internal",
    );
}

function parseRange(value: string): Range {
  return JSON.parse(value) as Range;
}

function docsToHits(
  docs: readonly ZVecDoc[],
  path: "fts" | "vector",
  storage: ZvecWorkspaceIndexStorage,
): StorageSearchHit[] {
  const hits: StorageSearchHit[] = [];

  for (const doc of docs) {
    const stored = storage.docToStoredFragment(doc);

    if (stored) {
      hits.push({
        ...stored,
        path,
        score: doc.score,
      });
    }
  }

  return hits;
}

function assertZvecStatusOrNotFound(
  status: ZVecStatus,
  operation: string,
  context: string,
): void {
  if (status.ok || status.code === "ZVEC_NOT_FOUND") {
    return;
  }

  assertZvecStatus(status, operation, context);
}

function readStringField(doc: ZVecDoc, field: string): string {
  return readStringFieldFromFields(doc.fields, field);
}

function readStringFieldFromFields(
  fields: Record<string, unknown>,
  field: string,
): string {
  const value = fields[field];

  if (typeof value === "string") {
    return value;
  }

  if (value === null || value === undefined) {
    return "";
  }

  return String(value);
}

function readNumberFieldFromFields(
  fields: Record<string, unknown>,
  field: string,
): number {
  const value = fields[field];

  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }

  if (typeof value === "string" && value.length > 0) {
    const parsed = Number.parseInt(value, 10);
    if (Number.isFinite(parsed)) {
      return parsed;
    }
  }

  return 0;
}

function readBooleanFieldFromFields(
  fields: Record<string, unknown>,
  field: string,
): boolean {
  const value = fields[field];

  if (typeof value === "boolean") {
    return value;
  }

  if (typeof value === "string") {
    return value === "true";
  }

  return Boolean(value);
}

function readNullableStringFieldFromFields(
  fields: Record<string, unknown>,
  field: string,
): string | null {
  const value = fields[field];

  if (value === null || value === undefined || value === "") {
    return null;
  }

  return String(value);
}

function readNullableNumberFieldFromFields(
  fields: Record<string, unknown>,
  field: string,
): number | null {
  const value = fields[field];

  if (typeof value === "number" && Number.isFinite(value)) {
    return value <= 0 ? null : value;
  }

  if (typeof value === "string" && value.length > 0) {
    const parsed = Number.parseInt(value, 10);
    return Number.isFinite(parsed) && parsed > 0 ? parsed : null;
  }

  return null;
}

function parseStringArray(value: string): string[] {
  try {
    const parsed = JSON.parse(value) as unknown;
    return Array.isArray(parsed)
      ? parsed.filter((item): item is string => typeof item === "string")
      : [];
  } catch {
    return [];
  }
}

function assertZvecStatus(
  status: ZVecStatus,
  operation: string,
  context: string,
): void {
  if (status.ok) {
    return;
  }

  throw new EngineError("zvec metadata operation failed", {
    code: "ZVEC_GREP.ENGINE.STORAGE.ZVEC_META_OPERATION_FAILED",
    context: `${operation}: ${context} code=${status.code} message=${status.message}`,
  });
}

function buildFilter(
  filter: StorageSearchFilter | undefined,
): string | undefined {
  if (!filter) {
    return undefined;
  }

  const clauses: string[] = [];

  if (filter.fileIds) {
    clauses.push(buildNonEmptyInFilter("file_id", filter.fileIds));
  }

  if (filter.groupIds) {
    clauses.push(buildNonEmptyInFilter("group", filter.groupIds));
  }

  if (filter.symbolNames) {
    clauses.push(buildNonEmptyInFilter("symbol_name", filter.symbolNames));
  }

  if (filter.symbolTypes) {
    clauses.push(buildNonEmptyInFilter("symbol_type", filter.symbolTypes));
  }

  return clauses.length > 0 ? clauses.join(" AND ") : undefined;
}

function buildNonEmptyInFilter(
  field: string,
  values: readonly string[],
): string {
  return values.length > 0 ? buildInFilter(field, values) : NO_MATCH_FILTER;
}

function buildInFilter(field: string, values: readonly string[]): string {
  if (values.length === 1) {
    return `${field} = ${quoteFilterString(values[0])}`;
  }

  return `(${values.map((value) => `${field} = ${quoteFilterString(value)}`).join(" OR ")})`;
}

function quoteFilterString(value: string): string {
  return `'${value.replaceAll("\\", "\\\\").replaceAll("'", "\\'")}'`;
}
