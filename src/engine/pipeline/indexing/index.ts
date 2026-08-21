import { lstat, readFile } from "node:fs/promises";
import {
  workspaceIndexDetail,
  detail,
  EngineError,
  errorDetails,
  isEngineError,
} from "../../errors.js";
import type { FileGraphInput, GraphStorage } from "../../graph/index.js";
import { extractFileGraph, fileGraphFromFragments } from "../../graph/index.js";
import type {
  EmbeddingModel,
  EmbeddingModelProgress,
  EmbeddingResult,
} from "../../models/index.js";
import type { WorkspaceIndexStorage } from "../../storage/index.js";
import type {
  WorkspaceIndexStatus,
  WorkspaceIndexInfo,
  Content,
  EntityFragment,
  FileScanDiagnostics,
  FileInfo,
  ImageFormat,
  IndexProgress,
  IndexEmbeddingProgress,
  IndexResult,
} from "../../types.js";
import { sha256Bytes } from "../../utils/hash.js";
import { normalizePath } from "../../utils/path.js";
import { ConcurrentTiming, TimingCollector } from "../../utils/timing.js";
import {
  analyzeForIndexing,
  type Source,
  vectorContentForFragment,
} from "../../extraction/index.js";
import {
  scanDirectoryPath,
  scanFilePath,
  scanRootPaths,
} from "./scanner/index.js";
import { indexChunkOptions } from "./input-budget.js";

export type IndexContext = {
  workspaceIndex: WorkspaceIndexInfo;
  storage: WorkspaceIndexStorage;
  graph?: GraphStorage;
  embeddingModel: EmbeddingModel;
  embeddingConcurrency?: number;
  onProgress?: (progress: IndexProgress) => void;
  signal?: AbortSignal;
};

type DiffResult = {
  added: FileInfo[];
  modified: FileInfo[];
  pending: FileInfo[];
  deleted: FileInfo[];
  unchanged: FileInfo[];
};

type PreparedFragment = {
  fragment: EntityFragment;
  embeddingContent: Content;
};

type PreparedFile = {
  file: FileInfo;
  fragments: PreparedFragment[];
  graph?: FileGraphInput;
};

type FailedPreparedFile = {
  file: FileInfo;
  failedReason: string;
};

type IndexStats = {
  filesIndexed: number;
  filesFailed: number;
  failedFiles: string[];
  failedFileReasons: string[];
  entitiesCreated: number;
};

type IndexPassResult = {
  filesScanned: number;
  scanDiagnostics: FileScanDiagnostics;
  diff: DiffResult;
  stats: IndexStats;
};

type IndexProgressBase = {
  filesSucceeded: number;
  filesTotal: number;
};

type IndexProgressReporter = (
  stats: IndexStats,
  detail?: string,
  embedding?: IndexEmbeddingProgress,
) => void;

const EMBEDDING_TRANSIENT_MAX_RETRIES = 3;
const EMBEDDING_RATE_LIMIT_MAX_RETRIES = 6;
const EMBEDDING_TRANSIENT_RETRY_BASE_DELAY_MS = 500;
const EMBEDDING_RATE_LIMIT_RETRY_BASE_DELAY_MS = 2000;
const EMBEDDING_TRANSIENT_RETRY_MAX_DELAY_MS = 8000;
const EMBEDDING_RATE_LIMIT_RETRY_MAX_DELAY_MS = 30000;
const EMBEDDING_RETRY_JITTER_MS = 500;
const EMBEDDING_SUCCESS_STREAK_MIN = 4;

type EmbeddingScheduler = {
  readonly taskConcurrency: number;
  run<T>(
    task: () => Promise<T>,
    signal?: AbortSignal,
    onError?: (error: unknown) => void,
  ): Promise<T>;
  recordSuccess(): void;
  recordRetryableFailure(retry: EmbeddingRetryDecision): void;
  snapshot(): IndexEmbeddingProgress;
};

type EmbeddingRetryClassification = {
  retryable: boolean;
  rateLimited: boolean;
  retryAfterMs?: number;
};

type EmbeddingRetryDecision = {
  rateLimited: boolean;
  delayMs: number;
};

type EmbeddingConcurrencyPolicy = {
  initial: number;
  min: number;
  max: number;
  adaptive: boolean;
};

export async function indexWorkspace(ctx: IndexContext): Promise<IndexResult> {
  try {
    return await indexWorkspaceUnchecked(ctx);
  } catch (error) {
    throw toEngineError(error, "Indexing workspace failed", {
      code: "ZVEC_GREP.ENGINE.INDEXING.WORKSPACE_FAILED",
      context: workspaceIndexContext(ctx.workspaceIndex),
    });
  }
}

export async function indexWorkspacePaths(
  ctx: IndexContext,
  changedPaths: readonly string[],
): Promise<IndexResult> {
  try {
    return await indexWorkspacePathsUnchecked(ctx, changedPaths);
  } catch (error) {
    throw toEngineError(error, "Indexing changed paths failed", {
      code: "ZVEC_GREP.ENGINE.INDEXING.WORKSPACE_FAILED",
      context: workspaceIndexContext(ctx.workspaceIndex),
    });
  }
}

export async function getWorkspaceIndexStatus(
  workspaceIndex: WorkspaceIndexInfo,
  storedFiles: readonly FileInfo[],
): Promise<WorkspaceIndexStatus> {
  try {
    const scan = await scanRootPaths(
      workspaceIndex.id,
      workspaceIndex.rootPaths,
      { knownFiles: storedFiles },
    );
    const diff = await computeDiffFromFiles(scan.files, storedFiles);
    const pendingFiles = storedFiles.filter(
      (file) => file.indexStatus?.indexedTime === null,
    );
    const failedFiles = pendingFiles.filter(
      (file) => file.indexStatus?.error !== undefined,
    );
    const indexedFiles = storedFiles.filter(
      (file) =>
        file.indexStatus?.indexedTime !== undefined &&
        file.indexStatus.indexedTime !== null,
    );
    const entitiesIndexed = indexedFiles.reduce(
      (count, file) => count + (file.indexStatus?.entityCount ?? 0),
      0,
    );
    const fragmentsTruncated = indexedFiles.reduce(
      (count, file) => count + (file.indexStatus?.truncatedFragmentCount ?? 0),
      0,
    );

    return {
      filesScanned: scan.files.length,
      filesStored: storedFiles.length,
      filesIndexed: indexedFiles.length,
      entitiesIndexed,
      fragmentsTruncated,
      filesPending: pendingFiles.length,
      filesFailed: failedFiles.length,
      filesAdded: diff.added.length,
      filesModified: diff.modified.length,
      filesDeleted: diff.deleted.length,
      filesUnchanged: diff.unchanged.length,
      pendingFiles,
      failedFiles,
      addedFiles: diff.added,
      modifiedFiles: diff.modified,
      deletedFiles: diff.deleted,
    };
  } catch (error) {
    throw toEngineError(error, "Inspecting workspace index status failed", {
      code: "ZVEC_GREP.ENGINE.INDEXING.STATUS_FAILED",
      context: workspaceIndexContext(workspaceIndex),
    });
  }
}

async function indexWorkspaceUnchecked(
  ctx: IndexContext,
): Promise<IndexResult> {
  const start = Date.now();
  const report = ctx.onProgress ?? (() => undefined);
  const timings = new TimingCollector();

  throwIfIndexCancelled(ctx);
  const firstPass = await runIndexPass(
    ctx,
    report,
    "Scanning files...",
    timings,
  );
  const passes = [firstPass];
  let progressBase: IndexProgressBase | undefined;
  if (firstPass.stats.filesFailed > 0) {
    const failed = firstPass.stats.filesFailed;
    progressBase = retryProgressBase(firstPass);
    report({
      phase: "scanning",
      filesTotal: progressBase.filesTotal,
      filesIndexed: progressBase.filesSucceeded,
      detail: `Retrying ${failed} failed ${failed === 1 ? "file" : "files"}...`,
    });
    passes.push(
      await runIndexPass(
        ctx,
        report,
        "Scanning retry candidates...",
        timings,
        progressBase,
      ),
    );
  }

  const finalPass = passes[passes.length - 1];
  throwIfIndexCancelled(ctx);
  reportIndexFinalizing(ctx, report, finalPass, progressBase);
  await timings.time("index_optimize", () => optimizeStorage(ctx));

  const result = buildIndexResult(ctx, passes, Date.now() - start, timings);

  if (result.filesFailed > 0) {
    report({ phase: "done", detail: "Indexing completed with failed files" });
    throw new EngineError(
      `Indexing completed with ${result.filesFailed} failed ${result.filesFailed === 1 ? "file" : "files"}`,
      {
        code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
        context: errorDetails([
          workspaceIndexDetail(ctx.workspaceIndex.name),
          detail("filesFailed", result.filesFailed),
          detail("filesScanned", result.filesScanned),
          detail(
            "failedFiles",
            summarizeFailedFiles(finalPass.stats.failedFiles),
          ),
          detail(
            "failedReasons",
            summarizeFailedFiles(finalPass.stats.failedFileReasons),
          ),
          detail(
            "hint",
            passes.length > 1
              ? "Retried failed files once automatically; if failures persist, fix the failed files or embedding configuration."
              : "If the failure was transient, rerun the same indexing command; if it persists, fix the failed files or embedding configuration.",
          ),
        ]),
      },
    );
  }

  report({ phase: "done", detail: "Indexing complete" });
  return result;
}

async function indexWorkspacePathsUnchecked(
  ctx: IndexContext,
  changedPaths: readonly string[],
): Promise<IndexResult> {
  const start = Date.now();
  const report = ctx.onProgress ?? (() => undefined);
  const timings = new TimingCollector();
  const normalizedPaths = [...new Set(changedPaths.map(normalizePath))];
  throwIfIndexCancelled(ctx);
  const firstPass = await runPathIndexPass(
    ctx,
    report,
    normalizedPaths,
    timings,
  );
  const passes = [firstPass];
  let progressBase: IndexProgressBase | undefined;
  if (firstPass.stats.filesFailed > 0) {
    progressBase = retryProgressBase(firstPass);
    passes.push(
      await runPathIndexPass(
        ctx,
        report,
        normalizedPaths,
        timings,
        progressBase,
      ),
    );
  }
  const finalPass = passes[passes.length - 1];
  throwIfIndexCancelled(ctx);
  reportIndexFinalizing(ctx, report, finalPass, progressBase);
  await timings.time("index_optimize", () => optimizeStorage(ctx));
  const result = buildIndexResult(ctx, passes, Date.now() - start, timings);
  if (result.filesFailed > 0) {
    throw new EngineError(
      `Indexing completed with ${result.filesFailed} failed files`,
      {
        code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
        context: workspaceIndexContext(ctx.workspaceIndex),
      },
    );
  }
  report({ phase: "done", detail: "Indexing complete" });
  return result;
}

async function runPathIndexPass(
  ctx: IndexContext,
  report: (progress: IndexProgress) => void,
  changedPaths: readonly string[],
  timings: TimingCollector,
  progressBase?: IndexProgressBase,
): Promise<IndexPassResult> {
  reportScanning(report, "Scanning changed paths...", progressBase);
  const scanned = await timings.time("index_scan_paths", async () => {
    const files: FileInfo[] = [];
    const diagnostics = emptyScanDiagnostics();
    for (const path of changedPaths) {
      throwIfIndexCancelled(ctx);
      const info = await lstat(path).catch(() => null);
      const scan = info?.isDirectory()
        ? await scanDirectoryPath(
            ctx.workspaceIndex.id,
            ctx.workspaceIndex.rootPaths,
            path,
            { signal: ctx.signal },
          )
        : await scanFilePath(
            ctx.workspaceIndex.id,
            ctx.workspaceIndex.rootPaths,
            path,
            {
              signal: ctx.signal,
            },
          );
      files.push(...scan.files);
      mergeScanDiagnostics(diagnostics, scan.diagnostics);
    }
    return {
      files: [...new Map(files.map((file) => [file.id, file])).values()],
      diagnostics,
    };
  });
  throwIfIndexCancelled(ctx);
  const exactExisting: FileInfo[] = [];
  const prefixPaths: string[] = [];
  for (const path of changedPaths) {
    const exact = ctx.storage.getFileByPath(path);
    if (exact) {
      exactExisting.push(exact);
    } else {
      prefixPaths.push(path);
    }
  }
  const existing = [
    ...new Map(
      [
        ...exactExisting,
        ...ctx.storage.listFilesByPathPrefixes(prefixPaths),
      ].map((file) => [file.id, file]),
    ).values(),
  ];
  return runDiffPass(
    ctx,
    report,
    scanned.files,
    existing,
    timings,
    progressBase,
    scanned.diagnostics,
  );
}

async function runIndexPass(
  ctx: IndexContext,
  report: (progress: IndexProgress) => void,
  scanningDetail: string,
  timings: TimingCollector,
  progressBase?: IndexProgressBase,
): Promise<IndexPassResult> {
  reportScanning(report, scanningDetail, progressBase);
  const existing = ctx.storage.listFiles();
  const scan = await timings.time("index_scan", () =>
    scanRootPaths(ctx.workspaceIndex.id, ctx.workspaceIndex.rootPaths, {
      signal: ctx.signal,
      knownFiles: existing,
    }),
  );
  throwIfIndexCancelled(ctx);
  return runDiffPass(
    ctx,
    report,
    scan.files,
    existing,
    timings,
    progressBase,
    scan.diagnostics,
  );
}

async function runDiffPass(
  ctx: IndexContext,
  report: (progress: IndexProgress) => void,
  scannedFiles: readonly FileInfo[],
  existingFiles: readonly FileInfo[],
  timings: TimingCollector,
  progressBase?: IndexProgressBase,
  scanDiagnostics: FileScanDiagnostics = emptyScanDiagnostics(),
): Promise<IndexPassResult> {
  const diff = await timings.time("index_diff", () =>
    computeDiffFromFiles(scannedFiles, existingFiles),
  );
  throwIfIndexCancelled(ctx);
  const pending = [...diff.added, ...diff.modified, ...diff.pending];

  report({
    phase: "scanning",
    filesTotal: progressBase?.filesTotal ?? scannedFiles.length,
    filesIndexed: progressBase?.filesSucceeded ?? 0,
    detail: `${diff.added.length} added, ${diff.modified.length} modified, ${diff.pending.length} pending, ${diff.deleted.length} deleted, ${diff.unchanged.length} unchanged`,
  });

  timings.timeSync("index_delete_stale", () => {
    for (const file of diff.deleted) {
      throwIfIndexCancelled(ctx);
      try {
        if (ctx.graph?.available) {
          ctx.graph.deleteFileGraph(file.id);
        }
        ctx.storage.deleteFile(file.id);
      } catch (error) {
        throw toEngineError(
          error,
          "Indexing failed to delete stale file records",
          {
            code: "ZVEC_GREP.ENGINE.INDEXING.DELETE_FILE_FAILED",
            context: fileContext(file),
          },
        );
      }
    }
  });

  const reportIndexing: IndexProgressReporter = (
    currentStats,
    detail,
    embedding,
  ) => {
    report({
      phase: "indexing",
      filesTotal: progressBase?.filesTotal ?? pending.length,
      filesIndexed:
        (progressBase?.filesSucceeded ?? 0) +
        currentStats.filesIndexed +
        currentStats.filesFailed,
      filesFailed: currentStats.filesFailed,
      detail,
      embedding,
    });
  };

  reportIndexing({
    filesIndexed: 0,
    filesFailed: 0,
    failedFiles: [],
    failedFileReasons: [],
    entitiesCreated: 0,
  });

  const stats = await indexFiles(pending, ctx, reportIndexing, timings);
  throwIfIndexCancelled(ctx);

  return {
    filesScanned: scannedFiles.length,
    scanDiagnostics,
    diff,
    stats,
  };
}

function reportIndexFinalizing(
  ctx: IndexContext,
  report: (progress: IndexProgress) => void,
  pass: IndexPassResult,
  progressBase?: IndexProgressBase,
): void {
  report({
    phase: "indexing",
    filesTotal:
      progressBase?.filesTotal ??
      pass.diff.added.length +
        pass.diff.modified.length +
        pass.diff.pending.length,
    filesIndexed:
      (progressBase?.filesSucceeded ?? 0) +
      pass.stats.filesIndexed +
      pass.stats.filesFailed,
    filesFailed: pass.stats.filesFailed,
    detail: "finalizing index",
  });
}

function retryProgressBase(pass: IndexPassResult): IndexProgressBase {
  return {
    filesSucceeded: pass.stats.filesIndexed,
    filesTotal:
      pass.diff.added.length +
      pass.diff.modified.length +
      pass.diff.pending.length,
  };
}

function reportScanning(
  report: (progress: IndexProgress) => void,
  detail: string,
  progressBase?: IndexProgressBase,
): void {
  report({
    phase: "scanning",
    ...(progressBase
      ? {
          filesTotal: progressBase.filesTotal,
          filesIndexed: progressBase.filesSucceeded,
        }
      : {}),
    detail,
  });
}

function buildIndexResult(
  ctx: IndexContext,
  passes: readonly IndexPassResult[],
  durationMs: number,
  timings: TimingCollector,
): IndexResult {
  const firstPass = passes[0];
  const retryPasses = passes.slice(1);
  const finalPass = passes[passes.length - 1];

  return {
    filesScanned: finalPass.filesScanned,
    filesAdded:
      firstPass.diff.added.length +
      retryPasses.reduce((count, pass) => count + pass.diff.added.length, 0),
    filesModified:
      firstPass.diff.modified.length +
      retryPasses.reduce((count, pass) => count + pass.diff.modified.length, 0),
    filesPending:
      firstPass.diff.pending.length +
      retryPasses.reduce((count, pass) => count + pass.diff.pending.length, 0),
    filesDeleted:
      firstPass.diff.deleted.length +
      retryPasses.reduce((count, pass) => count + pass.diff.deleted.length, 0),
    filesUnchanged: firstPass.diff.unchanged.length,
    filesFailed: finalPass.stats.filesFailed,
    entitiesCreated: passes.reduce(
      (count, pass) => count + pass.stats.entitiesCreated,
      0,
    ),
    durationMs,
    timings: timings.entries(),
    ...(finalPass.scanDiagnostics.skippedFiles > 0
      ? { scanDiagnostics: finalPass.scanDiagnostics }
      : {}),
  };
}

const MAX_SKIPPED_FILE_SAMPLES = 20;

function emptyScanDiagnostics(): FileScanDiagnostics {
  return {
    skippedFiles: 0,
    skippedByReason: {
      empty: 0,
      too_large: 0,
      unsupported: 0,
      binary: 0,
    },
    skippedSamples: [],
  };
}

function mergeScanDiagnostics(
  target: FileScanDiagnostics,
  source: FileScanDiagnostics,
): void {
  target.skippedFiles += source.skippedFiles;
  for (const reason of [
    "empty",
    "too_large",
    "unsupported",
    "binary",
  ] as const) {
    target.skippedByReason[reason] += source.skippedByReason[reason];
  }
  const remaining = MAX_SKIPPED_FILE_SAMPLES - target.skippedSamples.length;
  if (remaining > 0) {
    target.skippedSamples.push(...source.skippedSamples.slice(0, remaining));
  }
}

async function optimizeStorage(ctx: IndexContext): Promise<void> {
  try {
    await ctx.storage.finalizeWrites();
    if (ctx.graph?.available) {
      await ctx.graph.resolvePending({ files: ctx.storage.listFiles() });
      await ctx.graph.checkpoint();
    }
  } catch (error) {
    throw toEngineError(error, "Indexing failed to finalize storage", {
      code: "ZVEC_GREP.ENGINE.INDEXING.OPTIMIZE_FAILED",
      context: workspaceIndexContext(ctx.workspaceIndex),
    });
  }
}

async function computeDiffFromFiles(
  scannedFiles: readonly FileInfo[],
  existingFiles: readonly FileInfo[],
): Promise<DiffResult> {
  const existingById = new Map(existingFiles.map((file) => [file.id, file]));
  const seen = new Set<string>();
  const added: FileInfo[] = [];
  const modified: FileInfo[] = [];
  const pending: FileInfo[] = [];
  const unchanged: FileInfo[] = [];

  for (const file of scannedFiles) {
    seen.add(file.id);
    const existing = existingById.get(file.id);

    if (!existing) {
      added.push(await withContentHash(file));
      continue;
    }

    if (existing.indexStatus?.indexedTime === null) {
      pending.push(await withContentHash(file));
      continue;
    }

    if (
      existing.sizeBytes === file.sizeBytes &&
      existing.lastModifiedTime === file.lastModifiedTime &&
      existing.contentHash
    ) {
      unchanged.push(existing);
      continue;
    }

    const hashed = await withContentHash(file);
    if (
      existing.sizeBytes === hashed.sizeBytes &&
      existing.contentHash === hashed.contentHash
    ) {
      unchanged.push(existing);
      continue;
    }

    modified.push(hashed);
  }

  const deleted = [...existingById.values()].filter(
    (file) => !seen.has(file.id),
  );

  return { added, modified, pending, deleted, unchanged };
}

async function indexFiles(
  files: readonly FileInfo[],
  ctx: IndexContext,
  onProgress: IndexProgressReporter,
  timings: TimingCollector,
): Promise<IndexStats> {
  const stats: IndexStats = {
    filesIndexed: 0,
    filesFailed: 0,
    failedFiles: [],
    failedFileReasons: [],
    entitiesCreated: 0,
  };
  const batchFiles: PreparedFile[] = [];
  let batchFragmentCount = 0;
  const embeddingScheduler = createEmbeddingScheduler(
    ctx.embeddingConcurrency,
    ctx.embeddingModel,
  );
  const embeddingTiming = new ConcurrentTiming(timings, "index_embedding");
  const runningEmbeddings = new Set<Promise<void>>();
  const reportEmbeddingProgress = (
    currentStats: IndexStats,
    detail: string,
  ): void => {
    onProgress(currentStats, detail, embeddingScheduler.snapshot());
  };

  const flushBatch = async (): Promise<void> => {
    throwIfIndexCancelled(ctx);
    if (batchFiles.length === 0) {
      return;
    }

    const filesToEmbed = batchFiles.splice(0);
    batchFragmentCount = 0;

    await scheduleEmbeddingTask(() =>
      embedAndCommitBatch(
        filesToEmbed,
        ctx,
        stats,
        onProgress,
        embeddingScheduler,
        timings,
        embeddingTiming,
      ),
    );
  };

  const scheduleEmbeddingTask = async (
    task: () => Promise<void>,
  ): Promise<void> => {
    throwIfIndexCancelled(ctx);
    const promise = task().finally(() => {
      runningEmbeddings.delete(promise);
    });

    runningEmbeddings.add(promise);

    if (runningEmbeddings.size >= embeddingScheduler.taskConcurrency) {
      await Promise.race(runningEmbeddings);
    }
  };

  try {
    for (const file of files) {
      throwIfIndexCancelled(ctx);
      onProgress(stats, `reading ${file.relativePath}`);
      const prepared = await timings.time("index_prepare", () =>
        prepareFile(file, ctx),
      );
      throwIfIndexCancelled(ctx);

      if ("failedReason" in prepared) {
        recordFileFailed(stats, file, prepared.failedReason);
        onProgress(stats, `failed ${file.relativePath}`);
        continue;
      }

      if (prepared.fragments.length === 0) {
        const committed = commitFile(prepared, [], ctx, stats);
        onProgress(stats, finishedFileDetail(committed, file.relativePath));
        continue;
      }

      if (
        prepared.fragments.length > ctx.embeddingModel.info.limits.maxBatchSize
      ) {
        await flushBatch();
        await scheduleEmbeddingTask(async () => {
          throwIfIndexCancelled(ctx);
          reportEmbeddingProgress(stats, `embedding ${file.relativePath}`);
          await embedAndCommitFile(
            prepared,
            ctx,
            stats,
            onProgress,
            embeddingScheduler,
            timings,
            embeddingTiming,
          );
        });
        continue;
      }

      if (
        batchFragmentCount > 0 &&
        batchFragmentCount + prepared.fragments.length >
          ctx.embeddingModel.info.limits.maxBatchSize
      ) {
        await flushBatch();
      }

      batchFiles.push(prepared);
      batchFragmentCount += prepared.fragments.length;

      if (batchFragmentCount === ctx.embeddingModel.info.limits.maxBatchSize) {
        await flushBatch();
      }
    }

    await flushBatch();
    await Promise.all(runningEmbeddings);
  } catch (error) {
    await Promise.allSettled(runningEmbeddings);
    throw error;
  }
  throwIfIndexCancelled(ctx);

  return stats;
}

async function prepareFile(
  file: FileInfo,
  ctx: IndexContext,
): Promise<PreparedFile | FailedPreparedFile> {
  try {
    throwIfIndexCancelled(ctx);
    const source = await readSource(file);
    const chunkOptions = indexChunkOptions(
      ctx.embeddingModel.info.limits.maxInputTokens,
      source.kind === "text" ? source.text : undefined,
    );
    const analysis = await analyzeForIndexing(source, chunkOptions);
    const extracted = analysis.fragments;
    throwIfIndexCancelled(ctx);
    const fragments = extracted
      .filter(({ fragment }) =>
        ctx.embeddingModel.info.inputKinds.includes(fragment.content.kind),
      )
      .map(({ fragment, embeddingSource }): PreparedFragment => ({
        fragment,
        embeddingContent: vectorContentForFragment(
          fragment,
          embeddingSource,
          chunkOptions.maxChunkChars,
        ),
      }));

    let graph: FileGraphInput | undefined;
    if (ctx.graph?.available && source.kind === "text") {
      graph = await extractFileGraph(
        source,
        fragments.map(({ fragment }) => fragment),
        analysis,
      );
    }

    return { file, fragments, graph };
  } catch (error) {
    if (indexIsCancelled(ctx)) {
      throw indexCancellationError(ctx);
    }
    return {
      file,
      failedReason: markFileFailed(ctx, file, error, "prepare"),
    };
  }
}

async function embedAndCommitBatch(
  files: readonly PreparedFile[],
  ctx: IndexContext,
  stats: IndexStats,
  onProgress: IndexProgressReporter,
  embeddingScheduler: EmbeddingScheduler,
  timings: TimingCollector,
  embeddingTiming: ConcurrentTiming,
): Promise<void> {
  try {
    throwIfIndexCancelled(ctx);
    const contents = files.flatMap((file) =>
      file.fragments.map((fragment) => fragment.embeddingContent),
    );
    onProgress(
      stats,
      `embedding ${describePreparedFiles(files)}`,
      embeddingScheduler.snapshot(),
    );
    const embedding = await embeddingTiming.time(() =>
      embedContentsWithRetry(
        contents,
        ctx.embeddingModel,
        embeddingScheduler,
        ctx.signal,
        (progress) =>
          reportModelDownloadProgress(
            stats,
            progress,
            onProgress,
            embeddingScheduler,
          ),
      ),
    );
    const truncatedInputIndexes = new Set(embedding.truncated);
    throwIfIndexCancelled(ctx);
    let offset = 0;

    for (const file of files) {
      throwIfIndexCancelled(ctx);
      const end = offset + file.fragments.length;
      const fileVectors = embedding.vectors.slice(offset, end);
      let truncatedFragmentCount = 0;
      for (let index = offset; index < end; index++) {
        if (truncatedInputIndexes.has(index)) {
          truncatedFragmentCount++;
        }
      }
      offset += file.fragments.length;
      const committed = timings.timeSync("index_commit", () =>
        commitFile(file, fileVectors, ctx, stats, truncatedFragmentCount),
      );
      onProgress(stats, finishedFileDetail(committed, file.file.relativePath));
    }
  } catch (error) {
    if (indexIsCancelled(ctx)) {
      throw indexCancellationError(ctx);
    }
    if (isRetryableEmbeddingError(error)) {
      for (const file of files) {
        const reason = markFileFailed(ctx, file.file, error, "embed");
        recordFileFailed(stats, file.file, reason);
        onProgress(stats, finishedFileDetail(false, file.file.relativePath));
      }
      return;
    }

    for (const file of files) {
      onProgress(
        stats,
        `embedding ${file.file.relativePath}`,
        embeddingScheduler.snapshot(),
      );
      await embedAndCommitFile(
        file,
        ctx,
        stats,
        onProgress,
        embeddingScheduler,
        timings,
        embeddingTiming,
      );
    }
  }
}

async function embedAndCommitFile(
  file: PreparedFile,
  ctx: IndexContext,
  stats: IndexStats,
  onProgress: IndexProgressReporter,
  embeddingScheduler: EmbeddingScheduler,
  timings: TimingCollector,
  embeddingTiming: ConcurrentTiming,
): Promise<void> {
  try {
    throwIfIndexCancelled(ctx);
    const embedding = await embeddingTiming.time(() =>
      embedFragments(
        file.fragments,
        ctx.embeddingModel,
        embeddingScheduler,
        ctx.signal,
        (progress) =>
          reportModelDownloadProgress(
            stats,
            progress,
            onProgress,
            embeddingScheduler,
          ),
      ),
    );
    throwIfIndexCancelled(ctx);
    const committed = timings.timeSync("index_commit", () =>
      commitFile(
        file,
        embedding.vectors,
        ctx,
        stats,
        embedding.truncated.length,
      ),
    );
    onProgress(stats, finishedFileDetail(committed, file.file.relativePath));
  } catch (error) {
    if (indexIsCancelled(ctx)) {
      throw indexCancellationError(ctx);
    }
    const reason = markFileFailed(ctx, file.file, error, "embed");
    recordFileFailed(stats, file.file, reason);
    onProgress(stats, finishedFileDetail(false, file.file.relativePath));
  }
}

function commitFile(
  file: PreparedFile,
  vectors: readonly number[][],
  ctx: IndexContext,
  stats: IndexStats,
  truncatedFragmentCount = 0,
): boolean {
  try {
    throwIfIndexCancelled(ctx);
    if (file.fragments.length !== vectors.length) {
      throw new EngineError(
        "Embedding returned mismatched entity/vector counts",
        {
          code: "ZVEC_GREP.ENGINE.STORAGE.ENTITY_VECTOR_COUNT_MISMATCH",
          context: `fileId=${file.file.id} fragmentCount=${file.fragments.length} vectorCount=${vectors.length}`,
        },
      );
    }
    ctx.storage.replaceFile(
      file.file,
      file.fragments.map(({ fragment }, index) => ({
        fragment,
        vector: vectors[index],
      })),
      {
        truncatedFragmentCount,
      },
    );
    if (ctx.graph?.available) {
      const graphInput =
        file.graph ??
        fileGraphFromFragments(
          file.file.id,
          file.fragments.map(({ fragment }) => fragment),
        );
      ctx.graph.upsertFileGraph(
        file.file.id,
        graphInput.nodes,
        graphInput.edges,
        graphInput.refs,
      );
    }
    stats.filesIndexed++;
    stats.entitiesCreated += countPublicEntities(
      file.fragments.map(({ fragment }) => fragment),
    );
    return true;
  } catch (error) {
    if (indexIsCancelled(ctx)) {
      throw indexCancellationError(ctx);
    }
    const reason = markFileFailed(ctx, file.file, error, "commit");
    recordFileFailed(stats, file.file, reason);
    return false;
  }
}

function throwIfIndexCancelled(ctx: IndexContext): void {
  if (!indexIsCancelled(ctx)) {
    return;
  }
  throw indexCancellationError(ctx);
}

function indexIsCancelled(ctx: IndexContext): boolean {
  return ctx.signal?.aborted === true;
}

function indexCancellationError(ctx: IndexContext): Error {
  return ctx.signal?.reason instanceof Error
    ? ctx.signal.reason
    : new EngineError("Indexing was cancelled.", {
        code: "ZVEC_GREP.ENGINE.INDEXING.CANCELLED",
        context: workspaceIndexContext(ctx.workspaceIndex),
      });
}

function recordFileFailed(
  stats: IndexStats,
  file: FileInfo,
  reason: string | undefined,
): void {
  stats.filesFailed++;
  stats.failedFiles.push(file.relativePath);
  if (reason) {
    stats.failedFileReasons.push(`${file.relativePath}: ${reason}`);
  }
}

function summarizeFailedFiles(files: readonly string[]): string {
  const shown = files.slice(0, 5);
  const remaining = files.length - shown.length;
  return remaining > 0
    ? `${shown.join(", ")} and ${remaining} more`
    : shown.join(", ");
}

function describePreparedFiles(files: readonly PreparedFile[]): string {
  if (files.length === 0) {
    return "0 files";
  }

  if (files.length === 1) {
    return files[0].file.relativePath;
  }

  return `${files.length} files, starting with ${files[0].file.relativePath}`;
}

function finishedFileDetail(succeeded: boolean, relativePath: string): string {
  return succeeded ? `indexed ${relativePath}` : `failed ${relativePath}`;
}

async function readSource(file: FileInfo): Promise<Source> {
  let bytes: Buffer;

  try {
    bytes = await readFile(file.absolutePath);
  } catch (error) {
    throw toEngineError(error, "Indexing failed to read source file", {
      code: "ZVEC_GREP.ENGINE.INDEXING.READ_SOURCE_FAILED",
      context: fileContext(file),
    });
  }

  if (file.kind === "image") {
    return {
      kind: "image",
      file,
      data: bytes,
      format: file.format as ImageFormat,
    };
  }

  return {
    kind: "text",
    file,
    text: bytes.toString("utf8"),
  };
}

async function embedFragments(
  fragments: readonly PreparedFragment[],
  model: EmbeddingModel,
  embeddingScheduler: EmbeddingScheduler,
  signal?: AbortSignal,
  onModelProgress?: (progress: EmbeddingModelProgress) => void,
): Promise<EmbeddingResult> {
  const batches: { start: number; fragments: PreparedFragment[] }[] = [];

  for (
    let start = 0;
    start < fragments.length;
    start += model.info.limits.maxBatchSize
  ) {
    const batch = fragments.slice(
      start,
      start + model.info.limits.maxBatchSize,
    );
    batches.push({ start, fragments: batch });
  }

  const vectors: number[][] = new Array(fragments.length);
  const truncatedInputIndexes: number[] = [];
  const results = await Promise.allSettled(
    batches.map((batch) =>
      embedFragmentBatch(
        batch.fragments,
        model,
        batch.start,
        embeddingScheduler,
        signal,
        onModelProgress,
      ),
    ),
  );
  let firstError: unknown;

  for (const [index, result] of results.entries()) {
    if (result.status === "rejected") {
      firstError ??= result.reason;
      continue;
    }

    const start = batches[index].start;
    for (const [offset, vector] of result.value.vectors.entries()) {
      vectors[start + offset] = vector;
    }
    truncatedInputIndexes.push(
      ...result.value.truncated.map((inputIndex) => start + inputIndex),
    );
  }

  if (firstError) {
    throw firstError;
  }

  return {
    vectors,
    truncated: truncatedInputIndexes,
  };
}

async function embedFragmentBatch(
  fragments: readonly PreparedFragment[],
  model: EmbeddingModel,
  startIndex: number,
  embeddingScheduler: EmbeddingScheduler,
  signal?: AbortSignal,
  onModelProgress?: (progress: EmbeddingModelProgress) => void,
): Promise<EmbeddingResult> {
  const contents = fragments.map((fragment) => fragment.embeddingContent);

  try {
    return await embedContentsWithRetry(
      contents,
      model,
      embeddingScheduler,
      signal,
      onModelProgress,
    );
  } catch (error) {
    if (fragments.length === 1 || isRetryableEmbeddingError(error)) {
      throw error;
    }

    return await embedFragmentBatchOneByOne(
      fragments,
      model,
      startIndex,
      embeddingScheduler,
      signal,
      onModelProgress,
    );
  }
}

async function embedFragmentBatchOneByOne(
  fragments: readonly PreparedFragment[],
  model: EmbeddingModel,
  startIndex: number,
  embeddingScheduler: EmbeddingScheduler,
  signal?: AbortSignal,
  onModelProgress?: (progress: EmbeddingModelProgress) => void,
): Promise<EmbeddingResult> {
  const vectors: number[][] = [];
  const truncatedInputIndexes: number[] = [];

  for (const [index, fragment] of fragments.entries()) {
    try {
      const result = await embedContentsWithRetry(
        [fragment.embeddingContent],
        model,
        embeddingScheduler,
        signal,
        onModelProgress,
      );
      vectors.push(result.vectors[0]);
      if (result.truncated.length > 0) {
        truncatedInputIndexes.push(index);
      }
    } catch (error) {
      throw new EngineError(
        "Embedding entity fragment failed after one-by-one fallback",
        {
          code: "ZVEC_GREP.ENGINE.INDEXING.EMBEDDING_FRAGMENT_FAILED",
          context: errorDetails([
            detail("model", model.info.reference),
            detail("fragmentId", fragment.fragment.id),
            detail("fragmentIndex", startIndex + index),
            isEngineError(error) ? detail("causeCode", error.code) : null,
            error instanceof Error ? detail("cause", error.message) : null,
            isEngineError(error) ? error.context : null,
          ]),
          cause: error,
        },
      );
    }
  }

  return {
    vectors,
    truncated: truncatedInputIndexes,
  };
}

async function embedContentsWithRetry(
  contents: readonly Content[],
  model: EmbeddingModel,
  embeddingScheduler: EmbeddingScheduler,
  signal?: AbortSignal,
  onModelProgress?: (progress: EmbeddingModelProgress) => void,
): Promise<EmbeddingResult> {
  let attempt = 0;

  while (true) {
    let retry = nonRetryableEmbeddingError();
    let delayMs = 0;

    try {
      throwIfAborted(signal);
      const result = await embeddingScheduler.run(
        () =>
          model.embed(contents, {
            purpose: "document",
            signal,
            onProgress: onModelProgress,
          }),
        signal,
        (error) => {
          retry = classifyEmbeddingRetry(error);
          delayMs = retryDelayMs(attempt, retry);
          if (retry.retryable) {
            embeddingScheduler.recordRetryableFailure({
              rateLimited: retry.rateLimited,
              delayMs,
            });
          }
        },
      );
      embeddingScheduler.recordSuccess();
      return result;
    } catch (error) {
      if (signal?.aborted) {
        throw signal.reason instanceof Error
          ? signal.reason
          : new Error("Embedding was cancelled.");
      }
      if (!retry.retryable) {
        retry = classifyEmbeddingRetry(error);
        delayMs = retryDelayMs(attempt, retry);
      }

      if (attempt >= maxRetryAttempts(retry) || !retry.retryable) {
        throw error;
      }

      await abortableDelay(delayMs, signal);
      attempt++;
    }
  }
}

function reportModelDownloadProgress(
  stats: IndexStats,
  progress: EmbeddingModelProgress,
  onProgress: IndexProgressReporter,
  embeddingScheduler: EmbeddingScheduler,
): void {
  onProgress(stats, `downloading ${progress.model}`, {
    ...embeddingScheduler.snapshot(),
    ...progress,
  });
}

function createEmbeddingScheduler(
  requestedConcurrency: number | undefined,
  model: EmbeddingModel,
): EmbeddingScheduler {
  return new AdaptiveEmbeddingScheduler(
    resolveEmbeddingConcurrencyPolicy(requestedConcurrency, model),
  );
}

function resolveEmbeddingConcurrencyPolicy(
  requestedConcurrency: number | undefined,
  model: EmbeddingModel,
): EmbeddingConcurrencyPolicy {
  if (
    requestedConcurrency !== undefined &&
    Number.isInteger(requestedConcurrency) &&
    requestedConcurrency > 0
  ) {
    return {
      initial: requestedConcurrency,
      min: 1,
      max: requestedConcurrency,
      adaptive: requestedConcurrency > 1,
    };
  }

  const remote = model.info.provider === "qwen";
  const multimodal = model.info.inputKinds.includes("image");
  const configuredLocalDefault = model.info.defaultConcurrency;
  const localDefault =
    configuredLocalDefault !== undefined &&
    Number.isInteger(configuredLocalDefault) &&
    configuredLocalDefault > 0
      ? configuredLocalDefault
      : 1;
  const initial = remote ? (multimodal ? 4 : 8) : localDefault;
  const max = remote ? (multimodal ? 8 : 12) : localDefault;
  const min = Math.min(initial, 4);

  return {
    initial,
    min,
    max: Math.max(initial, max),
    adaptive: max > 1,
  };
}

class AdaptiveEmbeddingScheduler implements EmbeddingScheduler {
  private active = 0;
  private cooldownUntil = 0;
  private currentConcurrency: number;
  private retryableFailures = 0;
  private successStreak = 0;
  private readonly queue: (() => void)[] = [];

  constructor(private readonly policy: EmbeddingConcurrencyPolicy) {
    this.currentConcurrency = policy.initial;
  }

  get taskConcurrency(): number {
    return this.policy.max;
  }

  async run<T>(
    task: () => Promise<T>,
    signal?: AbortSignal,
    onError?: (error: unknown) => void,
  ): Promise<T> {
    throwIfAborted(signal);
    await this.waitForCooldown(signal);
    await this.acquire(signal);

    try {
      await this.waitForCooldown(signal);
      throwIfAborted(signal);
      return await task();
    } catch (error) {
      onError?.(error);
      throw error;
    } finally {
      this.release();
    }
  }

  recordSuccess(): void {
    if (!this.policy.adaptive || this.currentConcurrency >= this.policy.max) {
      return;
    }

    this.successStreak++;
    if (
      this.successStreak <
      Math.max(EMBEDDING_SUCCESS_STREAK_MIN, this.currentConcurrency * 2)
    ) {
      return;
    }

    this.currentConcurrency++;
    this.successStreak = 0;
    this.drainQueue();
  }

  recordRetryableFailure(retry: EmbeddingRetryDecision): void {
    this.retryableFailures++;
    if (retry.rateLimited && retry.delayMs > 0) {
      this.cooldownUntil = Math.max(
        this.cooldownUntil,
        Date.now() + retry.delayMs,
      );
    }

    if (!this.policy.adaptive) {
      return;
    }

    this.currentConcurrency = Math.max(
      this.policy.min,
      Math.floor(this.currentConcurrency / 2),
    );
    this.successStreak = 0;
  }

  snapshot(): IndexEmbeddingProgress {
    return {
      concurrency: this.currentConcurrency,
      maxConcurrency: this.policy.max,
      retryableFailures: this.retryableFailures,
    };
  }

  private async waitForCooldown(signal?: AbortSignal): Promise<void> {
    const remaining = this.cooldownUntil - Date.now();
    if (remaining > 0) {
      await abortableDelay(remaining, signal);
    }
  }

  private async acquire(signal?: AbortSignal): Promise<void> {
    throwIfAborted(signal);
    if (this.active < this.currentConcurrency) {
      this.active++;
      return;
    }

    await new Promise<void>((resolve, reject) => {
      const queued = () => {
        signal?.removeEventListener("abort", abort);
        resolve();
      };
      const abort = () => {
        const index = this.queue.indexOf(queued);
        if (index >= 0) {
          this.queue.splice(index, 1);
        }
        reject(abortError(signal));
      };
      if (signal?.aborted) {
        reject(abortError(signal));
        return;
      }
      signal?.addEventListener("abort", abort, { once: true });
      this.queue.push(queued);
    });
  }

  private release(): void {
    this.active--;
    this.drainQueue();
  }

  private drainQueue(): void {
    while (this.active < this.currentConcurrency) {
      const resolve = this.queue.shift();
      if (!resolve) {
        return;
      }

      this.active++;
      resolve();
    }
  }
}

function oneLine(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function isRetryableEmbeddingError(error: unknown): boolean {
  return classifyEmbeddingRetry(error).retryable;
}

function classifyEmbeddingRetry(error: unknown): EmbeddingRetryClassification {
  const message = errorToMessage(error);
  const context =
    isEngineError(error) && error.context ? ` ${error.context}` : "";
  const text = `${message}${context}`;
  const status = httpStatusFromText(text);
  const rateLimited =
    status === 429 ||
    /rate limit|quota exceeded|too many requests|request rate increased too quickly/i.test(
      text,
    );
  const serverError =
    typeof status === "number" && status >= 500 && status <= 599;

  return {
    retryable: rateLimited || serverError,
    rateLimited,
    retryAfterMs: retryAfterMsFromText(text),
  };
}

function nonRetryableEmbeddingError(): EmbeddingRetryClassification {
  return {
    retryable: false,
    rateLimited: false,
  };
}

function maxRetryAttempts(retry: EmbeddingRetryClassification): number {
  return retry.rateLimited
    ? EMBEDDING_RATE_LIMIT_MAX_RETRIES
    : EMBEDDING_TRANSIENT_MAX_RETRIES;
}

function retryDelayMs(
  attempt: number,
  retry: EmbeddingRetryClassification,
): number {
  if (typeof retry.retryAfterMs === "number") {
    return retry.retryAfterMs;
  }

  const base = retry.rateLimited
    ? EMBEDDING_RATE_LIMIT_RETRY_BASE_DELAY_MS
    : EMBEDDING_TRANSIENT_RETRY_BASE_DELAY_MS;
  const max = retry.rateLimited
    ? EMBEDDING_RATE_LIMIT_RETRY_MAX_DELAY_MS
    : EMBEDDING_TRANSIENT_RETRY_MAX_DELAY_MS;
  const exponential = base * 2 ** attempt;
  const jitter = Math.floor(Math.random() * EMBEDDING_RETRY_JITTER_MS);

  return Math.min(exponential + jitter, max);
}

function httpStatusFromText(text: string): number | undefined {
  const match = /\bstatus=(\d{3})\b/i.exec(text);
  if (!match) {
    return undefined;
  }

  return Number(match[1]);
}

function retryAfterMsFromText(text: string): number | undefined {
  const milliseconds = /\bretryAfterMs=(\d+)\b/i.exec(text);
  if (milliseconds) {
    return Number(milliseconds[1]);
  }

  const seconds = /\bretryAfter=(\d+(?:\.\d+)?)\b/i.exec(text);
  if (seconds) {
    return Math.round(Number(seconds[1]) * 1000);
  }

  return undefined;
}

function abortableDelay(
  ms: number,
  signal: AbortSignal | undefined,
): Promise<void> {
  if (ms <= 0) {
    return Promise.resolve();
  }
  throwIfAborted(signal);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", abort);
      resolve();
    }, ms);
    const abort = () => {
      clearTimeout(timer);
      reject(abortError(signal));
    };
    signal?.addEventListener("abort", abort, { once: true });
  });
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw abortError(signal);
  }
}

function abortError(signal: AbortSignal | undefined): Error {
  return signal?.reason instanceof Error
    ? signal.reason
    : new Error("Embedding was cancelled.");
}

function countPublicEntities(fragments: readonly EntityFragment[]): number {
  let count = 0;

  for (const fragment of fragments) {
    if (!fragment.group || fragment.group === fragment.id) {
      count++;
    }
  }

  return count;
}

async function withContentHash(file: FileInfo): Promise<FileInfo> {
  let bytes: Buffer;

  try {
    bytes = await readFile(file.absolutePath);
  } catch (error) {
    throw toEngineError(error, "Indexing failed to compute file content hash", {
      code: "ZVEC_GREP.ENGINE.INDEXING.CONTENT_HASH_FAILED",
      context: fileContext(file),
    });
  }

  return {
    ...file,
    contentHash: sha256Bytes(bytes),
  };
}

function markFileFailed(
  ctx: IndexContext,
  file: FileInfo,
  error: unknown,
  stage: string,
): string {
  const reason = fileFailureReason(stage, error);

  try {
    if (ctx.graph?.available) {
      ctx.graph.deleteFileGraph(file.id);
    }
    ctx.storage.markFileFailed(file, reason);
    return reason;
  } catch (markError) {
    throw toEngineError(markError, "Indexing failed to record file failure", {
      code: "ZVEC_GREP.ENGINE.INDEXING.MARK_FILE_FAILED",
      context: `${fileContext(file)} stage=${stage} original=${reason}`,
    });
  }
}

function fileFailureReason(stage: string, error: unknown): string {
  return oneLine(`${stage}: ${errorToMessage(error)}`);
}

function toEngineError(
  error: unknown,
  message: string,
  options: { code: EngineError["code"]; context: string },
): EngineError {
  if (isEngineError(error)) {
    return error;
  }

  return new EngineError(message, {
    ...options,
    cause: error,
  });
}

function workspaceIndexContext(workspaceIndex: WorkspaceIndexInfo): string {
  return (
    errorDetails([
      detail("workspace_index_id", workspaceIndex.id),
      workspaceIndexDetail(workspaceIndex.name),
    ]) ?? ""
  );
}

function fileContext(file: FileInfo): string {
  return `fileId=${file.id} path=${file.relativePath}`;
}

function errorToMessage(error: unknown): string {
  if (isEngineError(error) && error.context) {
    return `${error.code}: ${error.message} (${error.context})`;
  }

  if (isEngineError(error)) {
    return `${error.code}: ${error.message}`;
  }

  return error instanceof Error ? error.message : String(error);
}
