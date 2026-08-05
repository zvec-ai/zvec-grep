import { join } from "node:path";
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
import { fileBelongsToRootPath } from "../pipeline/indexing/root-paths.js";
import {
  diagnoseEntitySearch,
  diagnoseFileSearch,
  searchPlanCollection,
} from "../pipeline/search/index.js";
import type { CollectionStorage, StoredEntity } from "../storage/index.js";
import { ZvecCollectionStorage } from "../storage/zvec.js";
import type {
  CollectionEmbeddingSchema,
  CollectionIndexStatus,
  CollectionInfo,
  EntitySearchDiagnosis,
  FileDiagnosis,
  FileInfo,
  IndexOptions,
  IndexResult,
  SearchPlan,
  SearchPlanResult,
} from "../types.js";
import { CURRENT_INDEX_VERSION } from "../types.js";
import { normalizePath } from "../utils/path.js";

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
          detail(
            "hint",
            'Recreate the index with the current zvec-grep version; run "zg index --rebuild" for a workspace index.',
          ),
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
