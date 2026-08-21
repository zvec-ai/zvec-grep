import {
  workspaceIndexDetail,
  detail,
  EngineError,
  errorDetails,
} from "../errors.js";
import type { EmbeddingModel } from "../models/index.js";
import {
  getWorkspaceIndexStatus,
  indexWorkspace,
  indexWorkspacePaths,
} from "../pipeline/indexing/index.js";
import { searchWorkspaceIndex } from "../pipeline/search/index.js";
import type { GraphReader, GraphStorage } from "../graph/index.js";
import { openGraphStorage } from "../graph/index.js";
import {
  createWorkspaceIndexStorage,
  resolveWorkspaceIndexLayout,
  type StoredEntity,
  type StorageSearchHit,
  type WorkspaceIndexStorage,
} from "../storage/index.js";
import type {
  WorkspaceIndexEmbeddingSchema,
  WorkspaceIndexStatus,
  WorkspaceIndexInfo,
  IndexOptions,
  IndexResult,
  SearchPlan,
  SearchPlanResult,
} from "../types.js";
import { CURRENT_INDEX_VERSION } from "../types.js";

export type WorkspaceIndexOptions = {
  mode: "read" | "write";
  embeddingModel?: EmbeddingModel;
};

export class WorkspaceIndex {
  private readonly storage: WorkspaceIndexStorage;
  private readonly graphStorage: GraphStorage;
  private readonly embedding: WorkspaceIndexEmbeddingSchema;
  private readonly embeddingModel?: EmbeddingModel;
  private closed = false;

  constructor(
    readonly info: WorkspaceIndexInfo,
    options: WorkspaceIndexOptions,
  ) {
    this.embeddingModel = options.embeddingModel;
    this.embedding = requireWorkspaceIndexEmbedding(info, "open");
    this.validateIndexVersion();
    if (this.embeddingModel) {
      this.validateEmbeddingSchema(this.embeddingModel);
    }

    if (options.mode === "write") {
      this.storage = createWorkspaceIndexStorage({
        storagePath: info.path,
        readOnly: false,
        embedding: this.embedding,
      });
    } else {
      this.storage = createWorkspaceIndexStorage({
        storagePath: info.path,
        readOnly: true,
      });
    }
    this.graphStorage = openGraphStorage(
      resolveWorkspaceIndexLayout(info.path).graphPath,
      {
        readOnly: options.mode === "read",
      },
    );
  }

  get graph(): GraphReader {
    return this.graphStorage;
  }

  get name(): string {
    return this.info.name;
  }

  index(options: IndexOptions = {}): Promise<IndexResult> {
    if (this.storage.readOnly) {
      throw new EngineError("Cannot update a read-only workspace index", {
        code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.READ_ONLY",
        context: workspaceIndexOperationDetails(this.name, "index"),
      });
    }

    const embeddingModel = this.requireEmbeddingModel("index");

    const context = {
      workspaceIndex: this.info,
      embeddingModel,
      storage: this.storage,
      graph: this.graphStorage,
      embeddingConcurrency: options.embeddingConcurrency,
      onProgress: options.onProgress,
      signal: options.signal,
    };
    return options.changedPaths && options.changedPaths.length > 0
      ? indexWorkspacePaths(context, options.changedPaths)
      : indexWorkspace(context);
  }

  status(): Promise<WorkspaceIndexStatus> {
    return getWorkspaceIndexStatus(this.info, this.storage.listFiles());
  }

  searchPlan(plan: SearchPlan): Promise<SearchPlanResult> {
    return searchWorkspaceIndex(plan, {
      workspaceIndex: this.info,
      embeddingModel: this.embeddingModel,
      storage: this.storage,
      graph: this.graphStorage,
    });
  }

  getEntity(entityId: string) {
    return this.storage.getEntity(entityId);
  }

  listEntitiesByFile(
    fileId: string,
    options?: { limit?: number; offset?: number },
  ) {
    return this.storage.listEntitiesByFile(fileId, options);
  }

  findSymbolsByName(name: string, limit = 20) {
    const trimmed = name.trim();
    if (!trimmed || limit <= 0) return [];
    return uniqueStoredEntities(
      this.storage.searchFts(trimmed, limit, { symbolNames: [trimmed] }),
      this.storage,
      false,
    );
  }

  findSymbolsByQuery(query: string, limit = 40) {
    const trimmed = query.trim();
    if (!trimmed || limit <= 0) return [];
    return uniqueStoredEntities(
      this.storage.searchFts(trimmed, limit),
      this.storage,
      true,
    );
  }

  close(): void {
    if (this.closed) {
      return;
    }

    this.graphStorage.close();
    this.storage.close();
    this.closed = true;
  }

  private validateIndexVersion(): void {
    if (this.info.indexVersion !== CURRENT_INDEX_VERSION) {
      throw new EngineError("Workspace index version is not supported", {
        code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.VERSION_MISMATCH",
        context: errorDetails([
          workspaceIndexDetail(this.name),
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
        "Workspace index embedding provider does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_PROVIDER_MISMATCH",
          context: workspaceIndexMismatchDetails(
            this.name,
            expected.provider,
            current.info.provider,
          ),
        },
      );
    }

    if (expected.model !== current.info.name) {
      throw new EngineError(
        "Workspace index embedding model does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_MODEL_MISMATCH",
          context: workspaceIndexMismatchDetails(
            this.name,
            expected.model,
            current.info.name,
          ),
        },
      );
    }

    if (expected.dimension !== current.info.dimension) {
      throw new EngineError(
        "Workspace index embedding dimension does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_DIMENSION_MISMATCH",
          context: workspaceIndexMismatchDetails(
            this.name,
            expected.dimension,
            current.info.dimension,
          ),
        },
      );
    }

    if (expected.metric !== current.info.metric) {
      throw new EngineError(
        "Workspace index embedding metric does not match current model",
        {
          code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_METRIC_MISMATCH",
          context: workspaceIndexMismatchDetails(
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
        "Workspace index operation requires an embedding model",
        {
          code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_MODEL_REQUIRED",
          context: workspaceIndexOperationDetails(this.name, operation),
        },
      );
    }

    return this.embeddingModel;
  }
}

function uniqueStoredEntities(
  hits: readonly StorageSearchHit[],
  storage: WorkspaceIndexStorage,
  codeOnly: boolean,
): StoredEntity[] {
  const out: StoredEntity[] = [];
  const seen = new Set<string>();
  for (const hit of hits) {
    const id = hit.fragment.group ?? hit.fragment.id;
    if (seen.has(id)) continue;
    const stored = storage.getEntity(id);
    if (!stored || (codeOnly && stored.entity.metadata?.kind !== "code"))
      continue;
    seen.add(id);
    out.push(stored);
  }
  return out;
}

function workspaceIndexOperationDetails(
  name: string,
  operation: string,
): string | undefined {
  return errorDetails([
    workspaceIndexDetail(name),
    detail("operation", operation),
  ]);
}

function workspaceIndexMismatchDetails(
  name: string,
  expected: string | number,
  actual: string | number,
): string | undefined {
  return errorDetails([
    workspaceIndexDetail(name),
    detail("expected", expected),
    detail("actual", actual),
  ]);
}

export function isWorkspaceIndexed(
  info: WorkspaceIndexInfo | null | undefined,
): info is WorkspaceIndexInfo & {
  embedding: WorkspaceIndexEmbeddingSchema;
  indexVersion: number;
} {
  return (
    info?.embedding !== null &&
    info?.embedding !== undefined &&
    info.indexVersion !== null &&
    info.indexVersion !== undefined
  );
}

function requireWorkspaceIndexEmbedding(
  info: WorkspaceIndexInfo,
  operation: string,
): WorkspaceIndexEmbeddingSchema {
  if (isWorkspaceIndexed(info)) {
    return info.embedding;
  }

  throw new EngineError("Workspace index has not been built", {
    code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.MISSING",
    context: errorDetails([
      workspaceIndexDetail(info.name),
      detail("operation", operation),
      detail("hint", "Run zg index to build this index."),
    ]),
  });
}
