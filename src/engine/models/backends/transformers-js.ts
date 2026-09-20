import { join, resolve } from "node:path";
import { EngineError } from "../../errors.js";
import type { Content, TextContent } from "../../types.js";
import { defaultHome } from "../../utils/path.js";
import {
  BaseEmbeddingModel,
  type CreateEmbeddingModelOptions,
  type EmbeddingModelProgress,
  type EmbeddingModelInfo,
  type EmbeddingResult,
  type NormalizedEmbeddingOptions,
} from "../embeddings.js";
import type { TransformersJsEmbeddingCatalogEntry } from "../catalog.js";
import {
  resolveModelArtifacts,
  type ModelArtifactSource,
} from "../artifact-downloader.js";
import {
  INDEX_EMBEDDING_CONCURRENCY_ENV,
  normalizeLocalEmbeddingConcurrency,
  resolveLocalEmbeddingParallelism,
} from "../local-embedding-parallelism.js";
import { LocalEmbeddingQueue } from "../local-embedding-queue.js";
import {
  createModelDownloadProgressReporter,
  type ModelDownloadProgressReporter,
} from "../download-progress.js";

type TensorLike = {
  data: ArrayLike<number>;
  dims: readonly number[];
};

type TokenizerLike = {
  (
    text: string | string[],
    options: { truncation: true; max_length: number; padding: true },
  ):
    | {
        input_ids: {
          data: ArrayLike<number | bigint>;
          dims: readonly number[];
        };
        attention_mask: {
          data: ArrayLike<number | bigint>;
          dims: readonly number[];
        };
      }
    | Promise<{
        input_ids: {
          data: ArrayLike<number | bigint>;
          dims: readonly number[];
        };
        attention_mask: {
          data: ArrayLike<number | bigint>;
          dims: readonly number[];
        };
      }>;
  model_max_length: number;
};

type FeatureExtractionPipeline = {
  (
    texts: string[],
    options: {
      pooling: "mean" | "cls";
      normalize: boolean;
      truncation: true;
      max_length: number;
    },
  ): Promise<TensorLike>;
  tokenizer: TokenizerLike;
  dispose(): Promise<void>;
};

type TransformersJsModule = {
  pipeline(
    task: "feature-extraction",
    repo: string,
    options: {
      dtype: "fp32" | "q8" | "q4";
      local_files_only: true;
      session_options?: {
        executionProviders: TransformersJsExecutionProvider[];
      };
    },
  ): Promise<FeatureExtractionPipeline>;
};

type TransformersJsLoader = () => Promise<TransformersJsModule>;
type TransformersJsExecutionProvider = "cpu" | "webgpu" | "cuda" | "dml";
type ModelArtifactResolver = typeof resolveModelArtifacts;
type ResolvedModelArtifacts = Awaited<ReturnType<ModelArtifactResolver>>;
type TransformersJsDependencies = {
  loadRuntime: TransformersJsLoader;
  resolveArtifacts: ModelArtifactResolver;
};

type EmbeddingAttempt =
  | { ok: true; result: EmbeddingResult }
  | { ok: false; cause: unknown; canRetryOnCpu: boolean };

const DEFAULT_MODEL_CACHE_DIR = join(defaultHome(), "models");

async function defaultTransformersJsLoader(): Promise<TransformersJsModule> {
  try {
    return (await import("@huggingface/transformers")) as TransformersJsModule;
  } catch (cause) {
    throw new EngineError(
      "Transformers.js is required for this local embedding model",
      {
        code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_MISSING_DEPENDENCY",
        context: "Reinstall zvec-grep to restore @huggingface/transformers",
        cause,
      },
    );
  }
}

let defaultRuntimeImport: Promise<TransformersJsModule> | null = null;
const LOAD_FAILED = "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_LOAD_FAILED";

const defaultDependencies: TransformersJsDependencies = {
  loadRuntime() {
    defaultRuntimeImport ??= defaultTransformersJsLoader();
    return defaultRuntimeImport;
  },
  resolveArtifacts: resolveModelArtifacts,
};

export class TransformersJsEmbeddingModel extends BaseEmbeddingModel {
  readonly info: EmbeddingModelInfo;

  private readonly modelCacheDir: string;
  private readonly executionProvider: TransformersJsExecutionProvider | null;
  private readonly dependencies: TransformersJsDependencies;
  private readonly embeddingQueue: LocalEmbeddingQueue;
  private pipeline: FeatureExtractionPipeline | null = null;
  private pipelineLoadPromise: Promise<FeatureExtractionPipeline> | null = null;
  private pipelineLoadError: EngineError | null = null;
  private resolvedArtifacts: ResolvedModelArtifacts | null = null;
  private artifactResolutionPromise: Promise<ResolvedModelArtifacts> | null =
    null;
  private sourceFallbackWarningReported = false;
  private usingCpuFallback = false;
  private disposed = false;

  constructor(
    private readonly entry: TransformersJsEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
    dependencies: Partial<TransformersJsDependencies> = {},
  ) {
    super();
    const parallelism = normalizeLocalEmbeddingConcurrency(
      options.embeddingConcurrency,
    );
    this.info = {
      reference: entry.reference,
      provider: entry.provider,
      name: entry.model,
      dimension: entry.dimension,
      metric: entry.metric,
      defaultConcurrency: parallelism ?? 1,
      inputKinds: ["text"],
      limits: {
        maxBatchSize: entry.maxBatchSize,
        maxConcurrentBatches: parallelism ?? 1,
        maxInputTokens: entry.maxInputTokens,
      },
    };
    this.modelCacheDir = resolve(
      options.modelCacheDir ??
        process.env.ZVEC_GREP_MODEL_CACHE ??
        DEFAULT_MODEL_CACHE_DIR,
    );
    this.executionProvider = resolveExecutionProvider(options.device);
    this.dependencies = { ...defaultDependencies, ...dependencies };
    this.embeddingQueue = new LocalEmbeddingQueue(() =>
      resolveLocalEmbeddingParallelism({
        override: parallelism,
        gpu:
          this.executionProvider !== null && this.executionProvider !== "cpu",
      }),
    );
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult> {
    return await this.embedBatch(contents, options);
  }

  private async embedBatch(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult> {
    let attempt = await this.embeddingQueue.run(() =>
      this.runEmbeddingAttempt(contents, options),
    );
    if (attempt.ok) return attempt.result;

    let failure = attempt.cause;
    if (attempt.canRetryOnCpu) {
      try {
        // The failed attempt has released its slot. Wait for all other users of
        // the GPU pipeline before disposing it. Concurrent failures share one
        // CPU replacement and each retry their own batch once.
        await this.embeddingQueue.run(async () => {
          this.ensureNotDisposed();
          if (!this.usingCpuFallback) {
            await this.fallbackToCpu(failure, options.onProgress);
          }
        }, true);
        attempt = await this.embeddingQueue.run(() =>
          this.runEmbeddingAttempt(contents, options),
        );
        if (attempt.ok) return attempt.result;
        failure = attempt.cause;
      } catch (cause) {
        failure = cause;
      }
    }

    if (failure instanceof EngineError && failure.code === LOAD_FAILED) {
      throw failure;
    }
    throw new EngineError("Transformers.js embedding failed", {
      code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_EMBED_FAILED",
      context: `model=${this.entry.reference} repo=${this.entry.repo}${this.executionProvider && this.executionProvider !== "cpu" ? `; for GPU errors, retry with --device cpu${options.purpose === "document" ? ` or index with --index-embedding-concurrency 1 (environment fallback: ${INDEX_EMBEDDING_CONCURRENCY_ENV}=1)` : ""}` : ""}`,
      cause: failure,
    });
  }

  private async runEmbeddingAttempt(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingAttempt> {
    this.ensureNotDisposed();
    const texts = (contents as readonly TextContent[]).map((content) =>
      formatText(content.text, options.purpose, this.entry),
    );
    const pipeline = await this.ensurePipeline(options.onProgress);
    let truncatedInputIndexes: number[];
    try {
      truncatedInputIndexes = await findTruncatedInputIndexes(
        pipeline.tokenizer,
        texts,
        this.entry.maxInputTokens,
      );
    } catch (cause) {
      throw new EngineError("Transformers.js tokenization failed", {
        code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_TOKENIZATION_FAILED",
        context: `model=${this.entry.reference} repo=${this.entry.repo}`,
        cause,
      });
    }

    try {
      return {
        ok: true,
        result: await this.embedTexts(pipeline, texts, truncatedInputIndexes),
      };
    } catch (cause) {
      return {
        ok: false,
        cause,
        canRetryOnCpu:
          !this.usingCpuFallback &&
          this.executionProvider !== null &&
          this.executionProvider !== "cpu",
      };
    }
  }

  override async dispose(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    await this.embeddingQueue.run(async () => {
      const pipeline = this.pipeline;
      this.pipeline = null;
      this.pipelineLoadPromise = null;
      await pipeline?.dispose();
    }, true);
  }

  private async ensurePipeline(
    onProgress?: (progress: EmbeddingModelProgress) => void,
  ): Promise<FeatureExtractionPipeline> {
    if (this.pipeline) {
      return this.pipeline;
    }
    if (this.pipelineLoadError) {
      throw this.pipelineLoadError;
    }
    if (this.pipelineLoadPromise) {
      return await this.pipelineLoadPromise;
    }

    // Normalize the shared promise itself so concurrent callers also receive
    // the terminal error for artifact resolution and runtime import failures.
    this.pipelineLoadPromise = this.loadPipeline(onProgress).catch((cause) => {
      this.pipelineLoadError =
        cause instanceof EngineError && cause.code === LOAD_FAILED
          ? cause
          : new EngineError(
              `Transformers.js model initialization failed (${formatErrorMessage(cause)}). Check the model files and runtime configuration, then restart the process or daemon before retrying.`,
              {
                code: LOAD_FAILED,
                context: `model=${this.entry.reference} repo=${this.entry.repo}`,
                cause,
              },
            );
      throw this.pipelineLoadError;
    });
    try {
      this.pipeline = await this.pipelineLoadPromise;
      return this.pipeline;
    } finally {
      this.pipelineLoadPromise = null;
    }
  }

  private async loadPipeline(
    onProgress?: (progress: EmbeddingModelProgress) => void,
  ): Promise<FeatureExtractionPipeline> {
    const downloadProgress = createModelDownloadProgressReporter(
      this.entry.reference,
      onProgress,
    );
    downloadProgress.start();
    const resolvedArtifacts = await this.ensureArtifacts(downloadProgress);
    const runtime = await this.dependencies.loadRuntime();
    let pipeline: FeatureExtractionPipeline;
    const executionProvider = this.usingCpuFallback
      ? "cpu"
      : this.executionProvider;

    try {
      pipeline = await this.createPipeline(
        runtime,
        resolvedArtifacts.directory,
        executionProvider,
      );
    } catch (cause) {
      const recovery =
        executionProvider && executionProvider !== "cpu"
          ? "Restart the process or daemon and retry with --device cpu."
          : "Check the model files and runtime configuration, then restart the process or daemon before retrying.";
      const failure = new EngineError(
        `Transformers.js ${executionProvider ?? "cpu"} model initialization failed (${formatErrorMessage(cause)}). ${recovery}`,
        {
          code: LOAD_FAILED,
          context: `model=${this.entry.reference} repo=${this.entry.repo}`,
          cause,
        },
      );
      // Transformers.js 3.x retains its first session promise even on failure.
      // A CPU retry cannot recover a runtime whose first session failed. Do not
      // retry initialization here, or globally block unrelated models: pipeline
      // failures can also come from a tokenizer before ONNX session creation.
      if (!downloadProgress.warning(failure.message)) {
        process.stderr.write(`zvec-grep warning: ${failure.message}\n`);
      }
      throw failure;
    }

    pipeline.tokenizer.model_max_length = this.entry.maxInputTokens;
    downloadProgress.finish();
    return pipeline;
  }

  private async embedTexts(
    pipeline: FeatureExtractionPipeline,
    texts: string[],
    truncatedInputIndexes: number[],
  ): Promise<EmbeddingResult> {
    const tensor = await pipeline(texts, {
      pooling: this.entry.pooling,
      normalize: this.entry.normalize,
      truncation: true,
      max_length: this.entry.maxInputTokens,
    });
    return {
      vectors: tensorToVectors(tensor, texts.length, this.entry.dimension),
      truncated: truncatedInputIndexes,
    };
  }

  private async fallbackToCpu(
    cause: unknown,
    onProgress?: (progress: EmbeddingModelProgress) => void,
  ): Promise<boolean> {
    if (
      this.usingCpuFallback ||
      !this.executionProvider ||
      this.executionProvider === "cpu"
    ) {
      return false;
    }

    const warning = `Transformers.js ${this.executionProvider} embedding inference failed (${formatErrorMessage(cause)}), falling back to CPU.`;
    if (onProgress) {
      onProgress({
        stage: "warning",
        model: this.entry.reference,
        message: warning,
      });
    } else {
      process.stderr.write(`zvec-grep warning: ${warning}\n`);
    }
    this.usingCpuFallback = true;
    const pipeline = this.pipeline;
    this.pipeline = null;
    this.pipelineLoadPromise = null;
    await pipeline?.dispose();
    return true;
  }

  private async createPipeline(
    runtime: TransformersJsModule,
    modelDirectory: string,
    executionProvider: TransformersJsExecutionProvider | null,
  ): Promise<FeatureExtractionPipeline> {
    return await runtime.pipeline("feature-extraction", modelDirectory, {
      dtype: this.entry.dtype,
      local_files_only: true,
      ...(executionProvider
        ? { session_options: { executionProviders: [executionProvider] } }
        : {}),
    });
  }

  private async ensureArtifacts(
    downloadProgress: ModelDownloadProgressReporter,
  ): Promise<ResolvedModelArtifacts> {
    if (this.resolvedArtifacts) {
      return this.resolvedArtifacts;
    }
    if (this.artifactResolutionPromise) {
      return await this.artifactResolutionPromise;
    }

    const sources = this.createArtifactSources();
    this.artifactResolutionPromise = this.dependencies.resolveArtifacts({
      model: this.entry.reference,
      sources,
      artifacts: this.entry.artifacts,
      onDownloadPlan: (artifacts) => {
        downloadProgress.setDownloadPlan(artifacts);
      },
      onProgress: (progress) => {
        downloadProgress.report({
          artifact: progress.artifact,
          downloadedBytes: progress.downloadedBytes,
        });
      },
      onFallback: (warning) => {
        if (this.sourceFallbackWarningReported) {
          return;
        }
        this.sourceFallbackWarningReported = true;
        if (!downloadProgress.warning(warning)) {
          process.stderr.write(`zvec-grep warning: ${warning}\n`);
        }
      },
    });
    try {
      this.resolvedArtifacts = await this.artifactResolutionPromise;
      return this.resolvedArtifacts;
    } finally {
      this.artifactResolutionPromise = null;
    }
  }

  private createArtifactSources(): readonly ModelArtifactSource[] {
    const huggingFace = this.entry.sources.huggingFace;
    const modelScope = this.entry.sources.modelScope;
    return [
      {
        kind: "huggingface",
        repo: huggingFace.repo,
        revision: huggingFace.revision,
        cacheDirectory: resolve(
          this.modelCacheDir,
          huggingFace.repo,
          huggingFace.revision,
        ),
      },
      {
        kind: "modelscope",
        repo: modelScope.repo,
        revision: modelScope.revision,
        cacheDirectory: resolve(
          this.modelCacheDir,
          "modelscope",
          "transformers-js",
          modelScope.repo.replaceAll("/", "--"),
          modelScope.revision,
        ),
      },
    ];
  }

  private ensureNotDisposed(): void {
    if (this.disposed) {
      throw new EngineError("Transformers.js embedding model is disposed", {
        code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_DISPOSED",
        context: `model=${this.entry.reference}`,
      });
    }
  }
}

async function findTruncatedInputIndexes(
  tokenizer: TokenizerLike,
  texts: string[],
  maxInputTokens: number,
): Promise<number[]> {
  const encoded = await tokenizer(texts, {
    truncation: true,
    max_length: maxInputTokens + 1,
    padding: true,
  });
  const mask = encoded.attention_mask;
  const [batchSize, sequenceLength] = mask.dims;
  if (
    mask.dims.length !== 2 ||
    batchSize !== texts.length ||
    !Number.isInteger(sequenceLength) ||
    sequenceLength < 0 ||
    mask.data.length !== batchSize * sequenceLength
  ) {
    throw new Error("Transformers.js tokenizer returned an unexpected mask");
  }

  const truncatedInputIndexes: number[] = [];
  for (let inputIndex = 0; inputIndex < batchSize; inputIndex++) {
    let tokenCount = 0;
    const offset = inputIndex * sequenceLength;
    for (let tokenIndex = 0; tokenIndex < sequenceLength; tokenIndex++) {
      if (Number(mask.data[offset + tokenIndex]) !== 0) {
        tokenCount++;
      }
    }
    if (tokenCount > maxInputTokens) {
      truncatedInputIndexes.push(inputIndex);
    }
  }
  return truncatedInputIndexes;
}

function resolveExecutionProvider(
  device: CreateEmbeddingModelOptions["device"],
): TransformersJsExecutionProvider | null {
  if (device === undefined || device === "auto") {
    // Use the runtime's Node default (CPU). Platform support for an execution
    // provider does not imply that its hardware or shared libraries exist.
    return null;
  }
  if (device === "cpu") {
    return "cpu";
  }
  if (device === "metal" || device === "vulkan") {
    return "webgpu";
  }
  if (device === "cuda") {
    return "cuda";
  }

  return null;
}

function formatErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatText(
  text: string,
  purpose: NormalizedEmbeddingOptions["purpose"],
  entry: TransformersJsEmbeddingCatalogEntry,
): string {
  const prefix =
    purpose === "query"
      ? "queryPrefix" in entry
        ? entry.queryPrefix
        : undefined
      : "documentPrefix" in entry
        ? entry.documentPrefix
        : undefined;
  return prefix ? `${prefix}${text}` : text;
}

function tensorToVectors(
  tensor: TensorLike,
  count: number,
  dimension: number,
): number[][] {
  if (
    tensor.dims.length !== 2 ||
    tensor.dims[0] !== count ||
    tensor.dims[1] !== dimension ||
    tensor.data.length !== count * dimension
  ) {
    throw new EngineError("Transformers.js returned an unexpected tensor", {
      code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_INVALID_TENSOR",
      context: `expected=${count}x${dimension} actual=${tensor.dims.join("x")}`,
    });
  }

  return Array.from({ length: count }, (_, index) =>
    Array.from({ length: dimension }, (__, offset) => {
      const value = tensor.data[index * dimension + offset];
      if (!Number.isFinite(value)) {
        throw new EngineError(
          "Transformers.js returned a non-finite tensor value",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_INVALID_TENSOR",
            context: `index=${index} offset=${offset}`,
          },
        );
      }
      return value;
    }),
  );
}
