import {
  mkdirSync,
  readdirSync,
  readFileSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join, relative, resolve } from "node:path";
import {
  globalConfigPath,
  readGlobalConfig,
  type ZvecGrepGlobalConfig,
} from "../config.js";
import {
  Collection,
  CollectionRegistry,
  isCollectionIndexed,
} from "../collection/index.js";
import {
  collectionDetail,
  detail,
  EngineError,
  errorDetails,
} from "../errors/index.js";
import type { EmbeddingModel } from "../models/embeddings.js";
import {
  createEmbeddingModel,
  createEmbeddingModelFromReference,
} from "../models/index.js";
import type {
  CollectionEmbeddingSchema,
  CollectionIndexStatus,
  CollectionInfo,
  Content,
  FileInfo,
  IndexResult,
  RootPath,
  SearchHit,
  SearchPlan,
  SearchPlanResult,
} from "../types.js";
import {
  ANONYMOUS_COLLECTION_NAME,
  anonymousCollectionPath,
  anonymousHome,
  anonymousIndexLocation,
  findNearestAnonymousWorkspace,
  resolveZvecGrepRoot,
  type AnonymousIndexLocation,
} from "./root.js";
import { runLexicalFallback, runRgSearch } from "./lexical.js";
import { defaultHome } from "../utils/path.js";
import {
  acquireReadWriteLock,
  assertNoWriteLock,
  type FileLock,
} from "../utils/lock.js";
import { TimingCollector } from "../utils/timing.js";
import type {
  CreateZvecGrepOptions,
  ZvecGrep,
  ZvecGrepCollectionIndexOptions,
  ZvecGrepCollections,
  ZvecGrepContextItem,
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
  ZvecGrepInfoOptions,
  ZvecGrepInfoResult,
  ZvecGrepIndexOptions,
} from "./types.js";
import { enrichLexicalItemsWithStructure } from "./structure-enrichment.js";

const DEFAULT_CONTEXT_LIMIT = 10;
const DEFAULT_CONTEXT_TOTAL_LIMIT = 30;
const DEFAULT_LOCAL_EMBEDDING = "local/embeddinggemma-300m";
const MAX_RECOVERED_EMBEDDING_MODELS = 4;

export async function createZvecGrep(
  options: CreateZvecGrepOptions = {},
): Promise<ZvecGrep> {
  return new ZvecGrepService(options);
}

class ZvecGrepService implements ZvecGrep {
  readonly root: string;
  readonly collections: ZvecGrepCollections;
  private readonly embeddingModel?: EmbeddingModel;
  private readonly recoveredEmbeddingModels = new Map<string, EmbeddingModel>();
  private readonly retiredEmbeddingModels = new Set<EmbeddingModel>();
  private activeEmbeddingModelOperations = 0;
  private closed = false;

  constructor(private readonly options: CreateZvecGrepOptions) {
    this.root = resolveZvecGrepRoot(options.root);
    this.embeddingModel = options.embeddingModel;
    this.collections = {
      list: () => this.listCollections(),
      info: (name) => this.collectionInfo(name),
      status: (name) => this.collectionStatus(name),
      index: (name, paths, indexOptions) =>
        this.indexCollection(name, paths, indexOptions),
      remove: (name) => this.removeCollection(name),
    };
  }

  async index(options: ZvecGrepIndexOptions = {}): Promise<IndexResult> {
    this.ensureOpen();
    const root = resolveZvecGrepRoot(options.root ?? this.root);
    const location = anonymousIndexLocation(root);
    return await this.withEmbeddingModelOperation(() =>
      withHomeWriteLock(
        location.home,
        options.rebuild ? "index.rebuild" : "index",
        async () => {
          const existing = readCollectionInfo(
            location.home,
            ANONYMOUS_COLLECTION_NAME,
          );
          const embeddingModel = this.embeddingModelForIndex(
            existing,
            location.home,
            "index",
          );
          const registry = new CollectionRegistry(
            location.home,
            embeddingModel,
          );

          try {
            const rootPaths = resolveIndexRootPaths(
              existing,
              options.rootPaths,
              root,
              {
                resetPaths: options.resetPaths === true,
                includePaths: options.includePaths,
                excludePaths: options.excludePaths,
              },
            );

            if (options.rebuild) {
              registry.remove(ANONYMOUS_COLLECTION_NAME);
            }

            const existingAfterRebuild = registry.get(
              ANONYMOUS_COLLECTION_NAME,
            );
            if (isCollectionIndexed(existingAfterRebuild)) {
              assertCollectionEmbeddingMatchesCurrentModel(
                existingAfterRebuild,
                embeddingModel,
                "zg --index --rebuild",
              );
            }
            registry.prepareIndex(
              ANONYMOUS_COLLECTION_NAME,
              rootPaths,
              anonymousCollectionPath(root),
            );

            return await registry.open(ANONYMOUS_COLLECTION_NAME).index({
              rebuild: false,
              embeddingConcurrency: options.embeddingConcurrency,
              onProgress: options.onProgress,
            });
          } finally {
            registry.close();
          }
        },
      ),
    );
  }

  async disableIndex(
    options: ZvecGrepInfoOptions = {},
  ): Promise<ZvecGrepInfoResult> {
    this.ensureOpen();
    const root = resolveZvecGrepRoot(options.root ?? this.root);
    const location = anonymousIndexLocation(root);

    await withHomeWriteLock(location.home, "index.disable", async () => {
      const registry = new CollectionRegistry(location.home, undefined);
      try {
        registry.disableIndex(
          ANONYMOUS_COLLECTION_NAME,
          [root],
          location.collectionPath,
        );
      } finally {
        registry.close();
      }
    });

    return this.info({ root });
  }

  async context(
    options: ZvecGrepContextOptions,
  ): Promise<ZvecGrepContextResult> {
    this.ensureOpen();
    return await this.withEmbeddingModelOperation(async () => {
      const timings = new TimingCollector();
      const result = await timings.time("total", () =>
        this.contextWithTimings(options, timings),
      );
      return withContextTimings(result, timings);
    });
  }

  private async contextWithTimings(
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    const request = normalizeContextRequest(options);

    if (options.collection) {
      return this.contextFromNamedCollection(request, options, timings);
    }

    const startRoot = resolveZvecGrepRoot(options.root ?? this.root);
    if (options.rg) {
      return this.contextFromRg(startRoot, request, options, timings);
    }

    assertNearestAnonymousHomeUnlocked(startRoot, "context");
    const nearest = findNearestAnonymousCollection(startRoot);
    if (nearest) {
      const { location, info } = nearest;
      if (info.indexPolicy === "disabled") {
        throw anonymousIndexDisabledError(location.root);
      }

      if (!isCollectionIndexed(info)) {
        throw anonymousIndexMissingError(
          location.root,
          info.indexPolicy ?? "enabled",
        );
      }

      if (options.autoUpdate !== false) {
        await this.refreshAnonymousIndexForContext(location, options, timings);
      }

      return await withHomeReadLock(location.home, "context", () =>
        this.contextFromAnonymousIndex(location, request, options, timings),
      );
    }

    throw anonymousIndexMissingError(startRoot, "undecided");
  }

  async info(options: ZvecGrepInfoOptions = {}): Promise<ZvecGrepInfoResult> {
    this.ensureOpen();
    const startRoot = resolveZvecGrepRoot(options.root ?? this.root);
    assertNearestAnonymousHomeUnlocked(startRoot, "info");
    const nearest = findNearestAnonymousCollection(startRoot);

    if (!nearest) {
      const location = anonymousIndexLocation(startRoot);

      return {
        root: startRoot,
        indexed: false,
        indexPolicy: "undecided",
        home: location.home,
        indexPath: location.collectionPath,
        source: "unindexed",
        suggestion: "zg --index or zg --rg",
      };
    }

    return await withHomeReadLock(nearest.location.home, "info", async () => {
      const registry = new CollectionRegistry(
        nearest.location.home,
        undefined,
        true,
      );
      try {
        const collection = registry.get(ANONYMOUS_COLLECTION_NAME);
        const indexed =
          collection !== null &&
          collection.indexPolicy !== "disabled" &&
          isCollectionIndexed(collection);

        return {
          root: nearest.location.root,
          indexed,
          indexPolicy: collection?.indexPolicy ?? "undecided",
          home: nearest.location.home,
          indexPath: nearest.location.collectionPath,
          source: indexed ? "index" : "unindexed",
          collection: collection ?? undefined,
          status: indexed
            ? await registry.status(ANONYMOUS_COLLECTION_NAME)
            : null,
          suggestion: anonymousInfoSuggestion(collection),
        };
      } finally {
        registry.close();
      }
    });
  }

  async close(): Promise<void> {
    const models = new Set<EmbeddingModel>([
      ...(this.embeddingModel ? [this.embeddingModel] : []),
      ...this.recoveredEmbeddingModels.values(),
      ...this.retiredEmbeddingModels,
    ]);
    this.recoveredEmbeddingModels.clear();
    this.retiredEmbeddingModels.clear();

    for (const model of models) {
      await model.dispose();
    }

    this.closed = true;
  }

  private async listCollections(): Promise<CollectionInfo[]> {
    this.ensureOpen();
    return await withHomeReadLock(
      serviceHome(this.options),
      "collections.list",
      async () => {
        const registry = this.createRegistry(true);
        try {
          return registry.list();
        } finally {
          registry.close();
        }
      },
    );
  }

  private async collectionInfo(name: string): Promise<CollectionInfo | null> {
    this.ensureOpen();
    return await withHomeReadLock(
      serviceHome(this.options),
      "collections.info",
      async () => {
        const registry = this.createRegistry(true);
        try {
          return registry.get(name);
        } finally {
          registry.close();
        }
      },
    );
  }

  private async collectionStatus(name: string) {
    this.ensureOpen();
    return await withHomeReadLock(
      serviceHome(this.options),
      "collections.status",
      async () => {
        const registry = this.createRegistry(true);
        try {
          return await registry.status(name);
        } finally {
          registry.close();
        }
      },
    );
  }

  private async indexCollection(
    name: string,
    paths?: string | RootPath | readonly (string | RootPath)[],
    options: ZvecGrepCollectionIndexOptions = {},
  ): Promise<IndexResult> {
    this.ensureOpen();
    const home = serviceHome(this.options);
    return await this.withEmbeddingModelOperation(() =>
      withHomeWriteLock(
        home,
        options.rebuild ? "collections.index.rebuild" : "collections.index",
        async () => {
          const existing = readCollectionInfo(home, name);
          const embeddingModel = this.embeddingModelForIndex(
            existing,
            home,
            "collections.index",
          );
          const registry = this.createRegistry(false, embeddingModel);
          try {
            const requestedRootPaths =
              paths === undefined
                ? undefined
                : Array.isArray(paths)
                  ? paths
                  : [paths];
            const rootPaths = resolveIndexRootPaths(
              existing,
              requestedRootPaths,
              this.root,
              {
                resetPaths: options.resetPaths === true,
                includePaths: options.includePaths,
                excludePaths: options.excludePaths,
              },
            );

            if (options.rebuild) {
              registry.remove(name);
            }

            const existingAfterRebuild = registry.get(name);
            if (isCollectionIndexed(existingAfterRebuild)) {
              assertCollectionEmbeddingMatchesCurrentModel(
                existingAfterRebuild,
                embeddingModel,
                "zg --collections index <name> --rebuild",
              );
            }
            registry.prepareIndex(name, rootPaths);

            return await registry.open(name).index({
              embeddingConcurrency: options.embeddingConcurrency,
              onProgress: options.onProgress,
            });
          } finally {
            registry.close();
          }
        },
      ),
    );
  }

  private async removeCollection(name: string): Promise<boolean> {
    this.ensureOpen();
    return await withHomeWriteLock(
      serviceHome(this.options),
      "collections.remove",
      async () => {
        const registry = this.createRegistry(false);
        try {
          return registry.remove(name);
        } finally {
          registry.close();
        }
      },
    );
  }

  private async contextFromAnonymousIndex(
    location: AnonymousIndexLocation,
    request: NormalizedContextRequest,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    const registry = new CollectionRegistry(location.home, undefined, true);
    try {
      const info = registry.get(ANONYMOUS_COLLECTION_NAME);
      if (!info) {
        throw new EngineError("Collection not found", {
          code: "ZVEC_GREP.ENGINE.COLLECTION.NOT_FOUND",
        });
      }

      const collection = this.openCollectionForSearch(
        info,
        request,
        location.home,
      );
      try {
        return await this.contextFromCollection({
          root: location.root,
          request,
          collection,
          anonymous: true,
          options,
          timings,
        });
      } finally {
        collection.close();
      }
    } finally {
      registry.close();
    }
  }

  private async refreshAnonymousIndexForContext(
    location: AnonymousIndexLocation,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<void> {
    const needsRefresh = await timings.time("status_scan", () =>
      withHomeReadLock(location.home, "context.status", async () => {
        const registry = new CollectionRegistry(location.home, undefined, true);
        try {
          const status = await registry.status(ANONYMOUS_COLLECTION_NAME);
          return status ? collectionIndexStatusNeedsRefresh(status) : false;
        } finally {
          registry.close();
        }
      }),
    );

    if (!needsRefresh) {
      return;
    }

    await timings.time("auto_update", () =>
      withHomeWriteLock(location.home, "context.refresh", async () => {
        const existing = readCollectionInfo(
          location.home,
          ANONYMOUS_COLLECTION_NAME,
        );
        if (!existing) {
          return;
        }

        const stillNeedsRefresh = await timings.time(
          "refresh_status_scan",
          () =>
            collectionNeedsRefresh(location.home, ANONYMOUS_COLLECTION_NAME),
        );
        if (!stillNeedsRefresh) {
          return;
        }

        const embeddingModel = this.embeddingModelForIndex(
          existing,
          location.home,
          "context.refresh",
        );
        assertCollectionEmbeddingMatchesCurrentModel(
          existing,
          embeddingModel,
          "zg --index --rebuild",
        );

        const registry = new CollectionRegistry(location.home, embeddingModel);
        try {
          const result = await registry.open(ANONYMOUS_COLLECTION_NAME).index({
            embeddingConcurrency: options.embeddingConcurrency,
            onProgress: options.onAutoUpdateProgress,
          });
          timings.addEntries(result.timings, "auto_update_");
        } finally {
          registry.close();
        }
      }),
    );
  }

  private async contextFromNamedCollection(
    request: NormalizedContextRequest,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    const collectionName = options.collection!;
    return await withHomeReadLock(
      serviceHome(this.options),
      "collection.context",
      async () => {
        const registry = this.createRegistry(true, undefined);
        try {
          const info = registry.get(collectionName);
          if (!info) {
            throw new EngineError("Collection not found", {
              code: "ZVEC_GREP.ENGINE.COLLECTION.NOT_FOUND",
              context: errorDetails([collectionDetail(collectionName)]),
            });
          }

          const collection = this.openCollectionForSearch(
            info,
            request,
            registry.home,
          );
          const root =
            collection.info.rootPaths[0]?.absolutePath ??
            resolveZvecGrepRoot(options.root ?? this.root);

          try {
            return await this.contextFromCollection({
              root,
              request,
              collection,
              anonymous: false,
              options: {
                ...options,
                fallback: "disabled",
              },
              timings,
            });
          } finally {
            collection.close();
          }
        } finally {
          registry.close();
        }
      },
    );
  }

  private async contextFromCollection(input: {
    root: string;
    request: NormalizedContextRequest;
    collection: Collection;
    anonymous: boolean;
    options: ZvecGrepContextOptions;
    timings: TimingCollector;
  }): Promise<ZvecGrepContextResult> {
    const searches: SearchPlanResult[] = [];
    const limit = contextGroupLimit(
      input.options.limit,
      input.request.groups.length,
    );

    for (const group of input.request.groups) {
      const search = await input.collection.searchPlan({
        routes: group.routes,
        limit,
        trace: input.options.trace,
        preferSymbol: input.options.preferSymbol,
        symbolTypes: input.options.symbolTypes,
        includePaths: input.options.includePaths,
        excludePaths: input.options.excludePaths,
        modifiedAfter: input.options.modifiedAfter,
        modifiedBefore: input.options.modifiedBefore,
      });
      input.timings.addEntries(search.timings);
      searches.push(search);
    }

    const items = dedupeAndRerankContextItems(
      searches.flatMap((search) =>
        searchPlanToContextItems(search, input.root),
      ),
    );

    return {
      query: input.request.displayQuery,
      root: input.root,
      source: "index",
      coverage: "ranked_sample",
      collection: {
        id: input.collection.info.id,
        name: input.collection.info.name,
        path: input.collection.info.path,
        anonymous: input.anonymous,
      },
      items,
      diagnostics: {
        emptyReason: items.length === 0 ? "no_matches" : undefined,
        index: {
          hitsReturned: items.length,
          routes: searches.flatMap((search) => search.plan.routes),
        },
      },
    };
  }

  private async contextFromLexicalFallback(
    root: string,
    request: NormalizedContextRequest,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    const fallbackResults = [];
    const limit = contextGroupLimit(options.limit, request.groups.length);
    for (const query of request.fallbackQueries) {
      fallbackResults.push(
        await timings.time("fallback_search", () =>
          runLexicalFallback({
            root,
            query,
            limit,
            includePaths: options.includePaths,
            excludePaths: options.excludePaths,
            modifiedAfter: options.modifiedAfter,
            modifiedBefore: options.modifiedBefore,
            rgOptions: options.rgOptions,
          }),
        ),
      );
    }

    const items = dedupeAndRerankContextItems(
      fallbackResults.flatMap((fallback) => fallback.items),
    );
    const diagnostics = mergeFallbackDiagnostics(fallbackResults);

    return {
      query: request.displayQuery,
      root,
      source: "lexical_fallback",
      coverage: diagnostics.truncated
        ? "lexical_truncated"
        : "lexical_exhaustive",
      items,
      diagnostics: {
        emptyReason: items.length === 0 ? "no_matches" : undefined,
        fallback: diagnostics,
      },
    };
  }

  private async contextFromRg(
    root: string,
    request: NormalizedContextRequest,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    let rgResult;
    try {
      rgResult = await timings.time("rg_search", () =>
        runRgSearch({
          root,
          patterns: request.fallbackQueries,
          paths: options.rgPaths,
          limit: options.limit,
          includePaths: options.includePaths,
          excludePaths: options.excludePaths,
          modifiedAfter: options.modifiedAfter,
          modifiedBefore: options.modifiedBefore,
          rgOptions: options.rgOptions,
        }),
      );
    } catch (cause) {
      throw new EngineError("Search failed", {
        code: "ZVEC_GREP.ENGINE.SEARCH.FAILED",
        context: errorDetails([detail("source", "rg"), detail("root", root)]),
        cause,
      });
    }

    const structuralEnrichment = await timings.time(
      "structure_enrichment",
      () => enrichLexicalItemsWithStructure(root, rgResult.items),
    );
    const items = dedupeAndRerankContextItems(structuralEnrichment.items);
    const emptyReason =
      items.length === 0 ? rgEmptyReason(rgResult.diagnostics) : undefined;

    return {
      query: request.displayQuery,
      root,
      source: "rg",
      coverage: rgResult.diagnostics.truncated
        ? "lexical_truncated"
        : "lexical_exhaustive",
      items,
      diagnostics: {
        emptyReason,
        fallback: rgResult.diagnostics,
        structure: structuralEnrichment.diagnostics,
      },
    };
  }

  private createRegistry(
    readOnly: boolean,
    embeddingModel: EmbeddingModel | undefined = this.embeddingModel,
  ): CollectionRegistry {
    return new CollectionRegistry(this.options.home, embeddingModel, readOnly);
  }

  private openCollectionForSearch(
    info: CollectionInfo,
    request: NormalizedContextRequest,
    registryHome: string,
  ): Collection {
    return new Collection(
      info,
      this.embeddingModelForSearch(
        indexedEmbeddingSchema(info),
        request,
        registryHome,
      ),
      true,
      join(registryHome, "files.zvec"),
    );
  }

  private embeddingModelForSearch(
    schema: CollectionEmbeddingSchema,
    request: NormalizedContextRequest,
    registryHome: string,
  ): EmbeddingModel | undefined {
    if (!request.routes.some((route) => route.mode === "vector")) {
      return undefined;
    }

    if (this.embeddingModel) {
      return this.embeddingModel;
    }

    return this.recoverEmbeddingModel(schema, registryHome);
  }

  private embeddingModelForIndex(
    existing: CollectionInfo | null,
    registryHome: string,
    operation: string,
  ): EmbeddingModel {
    if (
      isCollectionIndexed(existing) &&
      !this.embeddingModel &&
      !this.options.embedding
    ) {
      return this.recoverEmbeddingModel(existing.embedding, registryHome);
    }

    const reference =
      this.options.embedding ??
      (this.options.defaultEmbedding === true
        ? DEFAULT_LOCAL_EMBEDDING
        : undefined);
    return (
      this.embeddingModel ??
      (reference
        ? this.embeddingModelFromReference(reference, registryHome)
        : undefined) ??
      this.configuredEmbeddingModel(registryHome) ??
      this.requireEmbeddingModel(operation)
    );
  }

  private recoverEmbeddingModel(
    schema: CollectionEmbeddingSchema,
    registryHome: string,
  ): EmbeddingModel {
    const config = readGlobalConfig();
    const options = providerOptions(
      this.options,
      this.root,
      registryHome,
      schema.provider,
      config,
    );
    const key = `${schema.provider}/${schema.model}/${providerOptionsFingerprint(options)}`;
    return this.cachedEmbeddingModel(key, () =>
      createEmbeddingModel(
        {
          provider: schema.provider,
          model: schema.model,
        },
        options,
      ),
    );
  }

  private configuredEmbeddingModel(
    registryHome: string,
  ): EmbeddingModel | undefined {
    const config = readGlobalConfig();
    const reference = config.defaults?.embedding;
    if (!reference) {
      return undefined;
    }

    return this.embeddingModelFromReference(reference, registryHome, config);
  }

  private embeddingModelFromReference(
    reference: string,
    registryHome: string,
    config: ZvecGrepGlobalConfig = readGlobalConfig(),
  ): EmbeddingModel {
    const provider = providerFromReference(reference);
    const options = providerOptions(
      this.options,
      this.root,
      registryHome,
      provider,
      config,
    );
    const key = `configured/${reference}/${providerOptionsFingerprint(options)}`;
    return this.cachedEmbeddingModel(key, () =>
      createEmbeddingModelFromReference(reference, options),
    );
  }

  private cachedEmbeddingModel(
    key: string,
    create: () => EmbeddingModel,
  ): EmbeddingModel {
    const cached = this.recoveredEmbeddingModels.get(key);
    if (cached) {
      this.recoveredEmbeddingModels.delete(key);
      this.recoveredEmbeddingModels.set(key, cached);
      return cached;
    }

    const model = create();
    this.recoveredEmbeddingModels.set(key, model);
    this.trimRecoveredEmbeddingModels();
    return model;
  }

  private trimRecoveredEmbeddingModels(): void {
    while (
      this.recoveredEmbeddingModels.size > MAX_RECOVERED_EMBEDDING_MODELS
    ) {
      const oldestKey = this.recoveredEmbeddingModels.keys().next().value;
      if (oldestKey === undefined) {
        return;
      }

      const model = this.recoveredEmbeddingModels.get(oldestKey);
      this.recoveredEmbeddingModels.delete(oldestKey);
      if (model) {
        this.retiredEmbeddingModels.add(model);
      }
    }
  }

  private async withEmbeddingModelOperation<T>(
    operation: () => Promise<T>,
  ): Promise<T> {
    this.activeEmbeddingModelOperations += 1;
    try {
      return await operation();
    } finally {
      this.activeEmbeddingModelOperations -= 1;
      if (this.activeEmbeddingModelOperations === 0) {
        await this.disposeRetiredEmbeddingModels();
      }
    }
  }

  private async disposeRetiredEmbeddingModels(): Promise<void> {
    const models = [...this.retiredEmbeddingModels];
    this.retiredEmbeddingModels.clear();
    for (const model of models) {
      await model.dispose();
    }
  }

  private requireEmbeddingModel(operation: string): EmbeddingModel {
    if (!this.embeddingModel) {
      throw new EngineError("zvec-grep operation requires an embedding model", {
        code: "ZVEC_GREP.ENGINE.SERVICE.EMBEDDING_MODEL_REQUIRED",
        context: errorDetails([
          detail("operation", operation),
          detail(
            "hint",
            `Pass "--embedding <model>", set ZVEC_GREP_EMBEDDING, or configure defaults.embedding in ${globalConfigPath()}. Existing indexes can rerun --index without --embedding to reuse the stored schema.`,
          ),
          detail(
            "examples",
            "local/embeddinggemma-300m, qwen/text-embedding-v4",
          ),
        ]),
      });
    }

    return this.embeddingModel;
  }

  private ensureOpen(): void {
    if (this.closed) {
      throw new EngineError("zvec-grep service is already closed", {
        code: "ZVEC_GREP.ENGINE.SERVICE.CLOSED",
      });
    }
  }
}

async function withHomeReadLock<T>(
  home: string,
  operation: string,
  task: () => Promise<T>,
): Promise<T> {
  const lock = acquireHomeLock(home, "read", operation);
  try {
    return await task();
  } finally {
    lock.release();
  }
}

async function withHomeWriteLock<T>(
  home: string,
  operation: string,
  task: () => Promise<T>,
): Promise<T> {
  const lock = acquireHomeLock(home, "write", operation);
  try {
    return await task();
  } finally {
    lock.release();
  }
}

async function collectionNeedsRefresh(
  home: string,
  collectionName: string,
): Promise<boolean> {
  const registry = new CollectionRegistry(home, undefined, true);
  try {
    const status = await registry.status(collectionName);
    return status ? collectionIndexStatusNeedsRefresh(status) : false;
  } finally {
    registry.close();
  }
}

function collectionIndexStatusNeedsRefresh(
  status: CollectionIndexStatus,
): boolean {
  return (
    status.filesAdded > 0 ||
    status.filesModified > 0 ||
    status.filesDeleted > 0 ||
    status.filesPending > 0 ||
    status.filesFailed > 0
  );
}

function acquireHomeLock(
  home: string,
  mode: "read" | "write",
  operation: string,
): FileLock {
  return acquireReadWriteLock(homeLockPath(home), mode, { operation });
}

function assertHomeUnlocked(home: string, operation: string): void {
  assertNoWriteLock(homeLockPath(home), operation);
}

function assertNearestAnonymousHomeUnlocked(
  start: string,
  operation: string,
): void {
  let current = resolve(start);

  while (true) {
    assertHomeUnlocked(anonymousHome(current), operation);

    const parent = dirname(current);
    if (parent === current) {
      return;
    }

    current = parent;
  }
}

function homeLockPath(home: string): string {
  return join(home, "locks", "home");
}

function serviceHome(options: CreateZvecGrepOptions): string {
  return options.home ?? defaultHome();
}

function rgEmptyReason(
  diagnostics: Awaited<ReturnType<typeof runRgSearch>>["diagnostics"],
): NonNullable<ZvecGrepContextResult["diagnostics"]["emptyReason"]> {
  return diagnostics.missingPaths &&
    diagnostics.missingPaths.length > 0 &&
    diagnostics.searchedPaths &&
    diagnostics.searchedPaths.length === 0
    ? "no_searchable_files"
    : "no_matches";
}

function withContextTimings(
  result: ZvecGrepContextResult,
  timings: TimingCollector,
): ZvecGrepContextResult {
  const entries = timings.entries();
  if (entries.length === 0) {
    return result;
  }

  return {
    ...result,
    diagnostics: {
      ...result.diagnostics,
      timings: entries,
    },
  };
}

type AnonymousCollectionRecord = {
  location: AnonymousIndexLocation;
  info: CollectionInfo;
};

function findNearestAnonymousCollection(
  start: string,
): AnonymousCollectionRecord | null {
  let current = resolve(start);

  while (true) {
    const location = anonymousIndexLocation(current);
    const info = readCollectionInfo(location.home, ANONYMOUS_COLLECTION_NAME);
    if (info) {
      return { location, info };
    }

    const parent = dirname(current);
    if (parent === current) {
      return null;
    }

    current = parent;
  }
}

function anonymousInfoSuggestion(
  collection: CollectionInfo | null,
): string | undefined {
  if (!collection) {
    return "zg --index or zg --rg";
  }

  if (collection.indexPolicy === "disabled") {
    return "zg --rg";
  }

  if (!isCollectionIndexed(collection)) {
    return "zg --index";
  }

  return undefined;
}

function anonymousIndexMissingError(
  root: string,
  policy: "undecided" | "enabled",
): EngineError {
  return new EngineError(
    "No anonymous zvec-grep index found for this workspace",
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.ANONYMOUS_INDEX_NOT_FOUND",
      context: errorDetails([
        detail("root", root),
        detail("policy", policy),
        detail(
          "hint",
          policy === "undecided"
            ? "Ask the user whether to build an index with zg --index, or use zg --rg for no-index search."
            : "Run zg --index to build the enabled workspace index, or use zg --rg for no-index search.",
        ),
        detail(
          "agent_prompt",
          policy === "undecided"
            ? "Ask the user whether this workspace should be indexed. If yes, run zg --index --embedding <model> with appropriate include/exclude filters; if no, run zg --disable-index. For immediate no-index search, use zg --rg."
            : "This workspace is marked index-enabled but has no built index. Ask before running zg --index if an embedding model or cost is involved; otherwise use zg --rg for immediate no-index search.",
        ),
      ]),
    },
  );
}

function anonymousIndexDisabledError(root: string): EngineError {
  return new EngineError(
    "Anonymous zvec-grep index is disabled for this workspace",
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.ANONYMOUS_INDEX_DISABLED",
      context: errorDetails([
        detail("root", root),
        detail("policy", "disabled"),
        detail(
          "hint",
          "Use zg --rg for no-index search. Run zg --index only if the user explicitly decides to index this workspace.",
        ),
        detail("agent_action", "do_not_build_index"),
      ]),
    },
  );
}

function resolveIndexRootPaths(
  existing: CollectionInfo | null,
  requested: readonly (string | RootPath)[] | undefined,
  fallbackRoot: string,
  options: {
    resetPaths: boolean;
    includePaths?: readonly string[];
    excludePaths?: readonly string[];
  },
): readonly (string | RootPath)[] {
  let rootPaths = requested ?? existing?.rootPaths ?? [fallbackRoot];

  if (options.resetPaths) {
    rootPaths = rootPaths.map(resetRootPathFilters);
  }

  if (
    options.includePaths !== undefined ||
    options.excludePaths !== undefined
  ) {
    rootPaths = rootPaths.map((rootPath) =>
      applyRootPathFilterOverrides(
        rootPath,
        options.includePaths,
        options.excludePaths,
      ),
    );
  }

  return rootPaths;
}

function resetRootPathFilters(rootPath: string | RootPath): string | RootPath {
  if (typeof rootPath === "string") {
    return rootPath;
  }

  return {
    absolutePath: rootPath.absolutePath,
    recursive: rootPath.recursive,
  };
}

function applyRootPathFilterOverrides(
  rootPath: string | RootPath,
  include: readonly string[] | undefined,
  exclude: readonly string[] | undefined,
): RootPath {
  const normalized =
    typeof rootPath === "string"
      ? {
          absolutePath: rootPath,
          recursive: true,
        }
      : rootPath;

  return {
    ...normalized,
    include: include ?? normalized.include,
    exclude: exclude ?? normalized.exclude,
  };
}

function readCollectionInfo(home: string, name: string): CollectionInfo | null {
  const registry = new CollectionRegistry(home, undefined, true);
  try {
    return registry.get(name);
  } finally {
    registry.close();
  }
}

function indexedEmbeddingSchema(
  info: CollectionInfo,
): CollectionEmbeddingSchema {
  if (isCollectionIndexed(info)) {
    return info.embedding;
  }

  throw new EngineError("zvec-grep index has not been built", {
    code: "ZVEC_GREP.ENGINE.SERVICE.INDEX_MISSING",
    context: errorDetails([
      collectionDetail(info.name),
      detail("hint", "Run zg --index to build this index."),
    ]),
  });
}

function providerOptions(
  options: CreateZvecGrepOptions,
  root: string,
  registryHome?: string,
  provider?: string,
  config: ZvecGrepGlobalConfig = readGlobalConfig(),
) {
  const providerConfig = provider ? config.providers?.[provider] : undefined;
  return {
    apiKey: options.apiKey ?? providerConfig?.apiKey ?? "",
    endpoint: options.endpoint ?? providerConfig?.endpoint,
    modelCacheDir:
      options.modelCacheDir ??
      process.env.ZVEC_GREP_MODEL_CACHE ??
      config.defaults?.modelCacheDir ??
      modelCacheDir(options, root, registryHome),
    llamaGpu: options.llamaGpu ?? config.defaults?.llamaGpu,
    embeddingParallelism:
      options.embeddingParallelism ?? config.defaults?.embeddingParallelism,
  };
}

function providerFromReference(reference: string): string | undefined {
  const separator = reference.indexOf("/");
  return separator > 0 ? reference.slice(0, separator) : undefined;
}

function providerOptionsFingerprint(options: {
  apiKey: string;
  endpoint?: string;
  modelCacheDir?: string;
  llamaGpu?: "auto" | "metal" | "vulkan" | "cuda" | false;
  embeddingParallelism?: number;
}): string {
  return createHash("sha256")
    .update(
      JSON.stringify([
        options.apiKey,
        options.endpoint,
        options.modelCacheDir,
        options.llamaGpu,
        options.embeddingParallelism,
      ]),
    )
    .digest("hex");
}

function assertCollectionEmbeddingMatchesCurrentModel(
  info: CollectionInfo | null,
  model: EmbeddingModel,
  rebuildCommand: string,
): void {
  if (!isCollectionIndexed(info)) {
    return;
  }

  const expected = info.embedding;
  const changed =
    expected.provider !== model.ref.provider ||
    expected.model !== model.ref.model ||
    expected.dimension !== model.dimension ||
    expected.metric !== model.metric;

  if (!changed) {
    return;
  }

  throw new EngineError(
    "Existing zvec-grep index uses a different embedding model",
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.EMBEDDING_SCHEMA_CHANGE_REQUIRES_REBUILD",
      context: errorDetails([
        collectionDetail(info.name),
        detail("existing", `${expected.provider}/${expected.model}`),
        detail("requested", `${model.ref.provider}/${model.ref.model}`),
        detail(
          "hint",
          `Run "${rebuildCommand}" to rebuild this index with the requested embedding model.`,
        ),
      ]),
    },
  );
}

function modelCacheDir(
  options: CreateZvecGrepOptions,
  root: string,
  registryHome?: string,
): string {
  if (options.modelCacheDir || process.env.ZVEC_GREP_MODEL_CACHE) {
    return options.modelCacheDir ?? process.env.ZVEC_GREP_MODEL_CACHE!;
  }

  const globalCache = join(options.home ?? defaultHome(), "models");
  const legacyCaches = [
    registryHome ? join(registryHome, "models") : undefined,
    join(anonymousHome(root), "models"),
  ];
  const globalHasModel = directoryHasGguf(globalCache);

  for (const legacyCache of legacyCaches) {
    if (legacyCache && directoryHasGguf(legacyCache) && !globalHasModel) {
      return legacyCache;
    }
  }

  if (globalHasModel || canWriteCacheDirectory(globalCache)) {
    return globalCache;
  }

  return join(anonymousHome(root), "models");
}

function directoryHasGguf(path: string): boolean {
  try {
    return readdirSync(path, { withFileTypes: true }).some(
      (entry) => entry.isFile() && entry.name.endsWith(".gguf"),
    );
  } catch {
    return false;
  }
}

function canWriteCacheDirectory(path: string): boolean {
  const probe = join(
    path,
    `.zvec-grep-write-test-${Date.now()}-${Math.random().toString(16).slice(2)}`,
  );
  try {
    mkdirSync(path, { recursive: true });
    writeFileSync(probe, "");
    unlinkSync(probe);
    return true;
  } catch {
    return false;
  }
}

function contextGroupLimit(
  limit: number | undefined,
  groupCount: number,
): number {
  if (limit !== undefined) {
    return limit;
  }

  const safeGroupCount = Math.max(1, groupCount);
  if (safeGroupCount <= 3) {
    return DEFAULT_CONTEXT_LIMIT;
  }

  return Math.max(1, Math.ceil(DEFAULT_CONTEXT_TOTAL_LIMIT / safeGroupCount));
}

type NormalizedContextRequest = {
  displayQuery: string;
  fallbackQueries: string[];
  routes: SearchPlan["routes"];
  groups: NormalizedContextGroup[];
};

type NormalizedContextGroup = {
  routes: SearchPlan["routes"];
  fallbackQuery: string;
};

function normalizeContextRequest(
  options: ZvecGrepContextOptions,
): NormalizedContextRequest {
  const primaryQueries = normalizePrimaryQueries(
    options.query,
    options.queries,
  );
  const extraRoutes = normalizeContextRoutes(options.routes ?? []);

  if (primaryQueries.length === 0 && extraRoutes.length === 0) {
    throw new EngineError(
      "zvec-grep context requires a non-empty query or route",
      {
        code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_QUERY",
      },
    );
  }

  const groups = contextGroups(primaryQueries, extraRoutes);
  const routes = groups.flatMap((group) => group.routes);
  const fallbackQueries = groups.map((group) => group.fallbackQuery);
  const displayQuery =
    primaryQueries.length > 0
      ? primaryQueries.join(" | ")
      : extraRoutes.map((route) => route.query).join(" | ");

  return {
    displayQuery,
    fallbackQueries,
    routes,
    groups,
  };
}

function normalizePrimaryQueries(
  query: string | undefined,
  queries: readonly string[] | undefined,
): string[] {
  return [query, ...(queries ?? [])].flatMap((value) => {
    const normalized = normalizeOptionalQuery(value);
    return normalized ? [normalized] : [];
  });
}

function normalizeOptionalQuery(query: string | undefined): string | undefined {
  const normalized = query?.trim() ?? "";
  return normalized.length > 0 ? normalized : undefined;
}

function normalizeContextRoutes(
  routes: readonly SearchPlan["routes"][number][],
): SearchPlan["routes"] {
  return routes.map((route, index) => {
    if (route.mode !== "fts" && route.mode !== "vector") {
      throw new EngineError("zvec-grep context route has an unsupported mode", {
        code: "ZVEC_GREP.ENGINE.SERVICE.INVALID_ROUTE_MODE",
        context: `routeIndex=${index} mode=${String(route.mode)}`,
      });
    }

    const query = route.query.trim();
    if (query.length === 0) {
      throw new EngineError(
        "zvec-grep context route requires a non-empty query",
        {
          code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_ROUTE_QUERY",
          context: `routeIndex=${index} mode=${route.mode}`,
        },
      );
    }

    return {
      mode: route.mode,
      query,
    };
  });
}

function contextGroups(
  primaryQueries: readonly string[],
  extraRoutes: SearchPlan["routes"],
): NormalizedContextGroup[] {
  return [
    ...primaryQueries.map((query) => ({
      routes: [
        { mode: "fts" as const, query },
        { mode: "vector" as const, query },
      ],
      fallbackQuery: query,
    })),
    ...extraRoutes.map((route) => ({
      routes: [route],
      fallbackQuery: route.query,
    })),
  ];
}

function searchPlanToContextItems(
  result: SearchPlanResult,
  root: string,
): ZvecGrepContextItem[] {
  return result.hits.map((hit) => {
    const target = contextItemTarget(hit);

    return {
      kind: "indexed_entity",
      rank: hit.rank,
      file: {
        absolutePath: hit.file.absolutePath,
        relativePath:
          hit.file.relativePath || relative(root, hit.file.absolutePath) || ".",
        rootPath: hit.file.rootPath,
      },
      range: hit.entity.range,
      excerptRange: target.excerptRange,
      content: contentToText(target.content),
      contentRole: target.contentRole,
      outline: target.outline,
      status: fileFreshnessStatus(hit.file),
      score: hit.score,
      matchedBy: hit.matchedBy,
      metadata: hit.entity.metadata,
      entityId: hit.entity.id,
      trace: hit.trace,
    };
  });
}

function dedupeAndRerankContextItems(
  items: readonly ZvecGrepContextItem[],
): ZvecGrepContextItem[] {
  const seen = new Set<string>();
  const deduped: ZvecGrepContextItem[] = [];

  for (const item of items) {
    const key = contextItemDedupeKey(item);
    if (seen.has(key)) {
      continue;
    }

    seen.add(key);
    deduped.push({
      ...item,
      rank: deduped.length + 1,
    });
  }

  return deduped;
}

function contextItemDedupeKey(item: ZvecGrepContextItem): string {
  if (item.entityId) {
    return `entity:${item.entityId}`;
  }

  return ["range", item.file.absolutePath, JSON.stringify(item.range)].join(
    ":",
  );
}

function mergeFallbackDiagnostics(
  results: readonly Awaited<ReturnType<typeof runLexicalFallback>>[],
) {
  const [first] = results;
  if (!first) {
    throw new EngineError(
      "zvec-grep lexical fallback requires at least one query",
      {
        code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_QUERY",
      },
    );
  }

  return {
    ...first.diagnostics,
    truncated: results.some((result) => result.diagnostics.truncated),
  };
}

type ContextItemTarget = {
  content: SearchHit["entity"]["content"];
  contentRole: "source" | "outline";
  excerptRange?: SearchHit["entity"]["range"];
  outline?: string;
};

function contextItemTarget(hit: SearchHit): ContextItemTarget {
  const evidence = hit.evidence.find((item) => !item.isEntity);
  const hasSeparateEvidence =
    evidence && !sameDisplayedContent(hit.entity, evidence);
  const content = hasSeparateEvidence ? evidence.content : hit.entity.content;
  const contentRole =
    hasSeparateEvidence || entityContentLooksLikeSource(hit.entity)
      ? "source"
      : "outline";
  const excerptRange = hasSeparateEvidence ? evidence.range : undefined;
  const outline =
    contentRole === "source" ? contextItemOutline(hit, evidence) : undefined;

  return {
    content,
    contentRole,
    excerptRange,
    outline,
  };
}

function entityContentLooksLikeSource(entity: SearchHit["entity"]): boolean {
  if (entity.content.kind !== "text" || entity.range.kind !== "text") {
    return true;
  }

  const expectedLines = entity.range.endLine - entity.range.startLine + 1;
  const actualLines = entity.content.text.split(/\r?\n/).length;

  return actualLines >= expectedLines;
}

function contextItemOutline(
  hit: SearchHit,
  evidence: SearchHit["evidence"][number] | undefined,
): string | undefined {
  if (hit.entity.content.kind !== "text") {
    return undefined;
  }

  const outline = hit.entity.content.text.trim();
  if (outline.length === 0) {
    return undefined;
  }

  if (!evidence) {
    return undefined;
  }

  if (evidence && sameDisplayedContent(hit.entity, evidence)) {
    return undefined;
  }

  if (!isUsefulOutline(hit.entity.metadata, outline)) {
    return undefined;
  }

  if (
    evidence?.content.kind === "text" &&
    evidence.content.text.trim() === outline
  ) {
    return undefined;
  }

  return outline;
}

function isUsefulOutline(
  metadata: SearchHit["entity"]["metadata"],
  outline: string,
): boolean {
  if (!metadata || metadata.kind !== "code") {
    return false;
  }

  return (
    metadata.symbolType === "class" ||
    metadata.symbolType === "interface" ||
    metadata.symbolType === "module" ||
    outline.includes("\ncalls:")
  );
}

function sameDisplayedContent(
  entity: Pick<SearchHit["entity"], "range" | "content">,
  evidence: Pick<SearchHit["entity"], "range" | "content">,
): boolean {
  return (
    JSON.stringify(entity.range) === JSON.stringify(evidence.range) &&
    contentEquals(entity.content, evidence.content)
  );
}

function contentEquals(left: Content, right: Content): boolean {
  if (left.kind !== right.kind) {
    return false;
  }

  if (left.kind === "image") {
    return (
      right.kind === "image" &&
      left.format === right.format &&
      left.data.byteLength === right.data.byteLength
    );
  }

  return right.kind === "text" && left.text === right.text;
}

function fileFreshnessStatus(file: FileInfo): "fresh" | "possibly_stale" {
  if (!file.indexStatus?.indexedTime) {
    return "possibly_stale";
  }

  try {
    const info = statSync(file.absolutePath, { throwIfNoEntry: false });
    if (!info || !info.isFile()) {
      return "possibly_stale";
    }

    if (file.indexStatus.indexedTime >= info.mtimeMs) {
      return "fresh";
    }

    if (
      file.contentHash &&
      sha256File(file.absolutePath) === file.contentHash
    ) {
      return "fresh";
    }
  } catch {
    return "possibly_stale";
  }

  return "possibly_stale";
}

function sha256File(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function contentToText(content: Content): string {
  if (content.kind === "text") {
    return content.text;
  }

  return `[image:${content.format} bytes=${content.data.byteLength}]`;
}
