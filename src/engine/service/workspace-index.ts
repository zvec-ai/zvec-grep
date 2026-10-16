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
import { WorkspaceBindingStore } from "../bindings.js";
import { dirname } from "node:path";
import {
  createWorkspaceIndexStorage,
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
  private readonly embedding: WorkspaceIndexEmbeddingSchema;
  private readonly embeddingModel?: EmbeddingModel;
  private readonly workspaceRoot: string;
  private readonly bindings: WorkspaceBindingStore;
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

    // The index home lives at <workspace>/.zvec-grep; the workspace root is
    // derived from the current location, never from persisted absolute paths.
    this.workspaceRoot = dirname(info.path);
    this.bindings = new WorkspaceBindingStore();

    if (options.mode === "write") {
      this.storage = createWorkspaceIndexStorage({
        storagePath: info.path,
        workspaceRoot: this.workspaceRoot,
        readOnly: false,
        embedding: this.embedding,
      });
    } else {
      this.storage = createWorkspaceIndexStorage({
        storagePath: info.path,
        workspaceRoot: this.workspaceRoot,
        readOnly: true,
      });
    }
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

    // An index is unverified until the host-local binding store proves the
    // current workspace binding was content-verified. Unverified indexes
    // reconcile: every matched file is hashed, unchanged content keeps its
    // vectors, and the binding is recorded only after success. --reconcile
    // forces the pass regardless of stored trust (the documented recovery
    // path for unsupported in-place restores); a forced run first drops the
    // prior verification so cancellation or failure leaves the index
    // unverified rather than trusted.
    if (options.reconcile === true) {
      this.bindings.invalidate(this.info.id, this.workspaceRoot);
    }
    const reconcile =
      options.reconcile === true ||
      !this.bindings.matches(this.info.id, this.workspaceRoot);
    const context = {
      workspaceIndex: this.info,
      embeddingModel,
      storage: this.storage,
      workspaceRoot: this.workspaceRoot,
      reconcile,
      embeddingConcurrency: options.embeddingConcurrency,
      onProgress: options.onProgress,
      signal: options.signal,
    };
    const run = reconcile
      ? // Reconciliation is always a complete pass: partial verification
        // cannot establish the binding.
        indexWorkspace(context)
      : options.changedPaths?.length
        ? indexWorkspacePaths(context, options.changedPaths)
        : indexWorkspace(context);
    return run.then((result) => {
      if (reconcile) {
        this.bindings.record(this.info.id, this.workspaceRoot);
      }
      return result;
    });
  }

  status(): Promise<WorkspaceIndexStatus> {
    return getWorkspaceIndexStatus(
      this.info,
      this.storage.listFiles(),
      this.workspaceRoot,
      { unverified: !this.bindings.matches(this.info.id, this.workspaceRoot) },
    );
  }

  searchPlan(plan: SearchPlan): Promise<SearchPlanResult> {
    return searchWorkspaceIndex(plan, {
      workspaceIndex: this.info,
      embeddingModel: this.embeddingModel,
      storage: this.storage,
    });
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
      throw new EngineError("Workspace index version is not supported", {
        code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.VERSION_MISMATCH",
        context: errorDetails([
          workspaceIndexDetail(this.name),
          detail("expected", CURRENT_INDEX_VERSION),
          detail("actual", this.info.indexVersion),
          detail(
            "hint",
            'Recreate the index with the current zvec-grep version; run "zg --index --rebuild" for a workspace index.',
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
      detail("hint", "Run zg --index to build this index."),
    ]),
  });
}
