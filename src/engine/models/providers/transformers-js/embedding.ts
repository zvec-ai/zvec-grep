import { join } from "node:path";
import { EngineError } from "../../../errors/index.js";
import type { Content, TextContent } from "../../../types.js";
import { defaultHome } from "../../../utils/path.js";
import {
  EmbeddingModel,
  type EmbeddingBatchResult,
  type EmbeddingLimits,
  type EmbeddingOptions,
  type EmbeddingVector,
} from "../../embeddings.js";
import type {
  ModelProviderOptions,
  TransformersJsEmbeddingCatalogEntry,
} from "../../types.js";

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
      cache_dir: string;
      revision: string;
      dtype: "fp32" | "q8" | "q4";
      session_options?: {
        executionProviders: TransformersJsExecutionProvider[];
      };
    },
  ): Promise<FeatureExtractionPipeline>;
};

type TransformersJsLoader = () => Promise<TransformersJsModule>;
type TransformersJsExecutionProvider = "cpu" | "webgpu" | "cuda" | "dml";

const DEFAULT_MODEL_CACHE_DIR = join(defaultHome(), "models");
let transformersJsImport: Promise<TransformersJsModule> | null = null;
let transformersJsLoader: TransformersJsLoader = defaultTransformersJsLoader;

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

async function loadTransformersJs(): Promise<TransformersJsModule> {
  transformersJsImport ??= transformersJsLoader();
  return await transformersJsImport;
}

export function setTransformersJsRuntimeForTesting(
  loader: TransformersJsLoader | null,
): void {
  transformersJsImport = null;
  transformersJsLoader = loader ?? defaultTransformersJsLoader;
}

export class TransformersJsEmbeddingModel extends EmbeddingModel {
  readonly ref;
  readonly dimension;
  readonly metric;
  readonly supportedContentKinds = ["text"] as const;
  readonly limits;
  override readonly recommendedIndexConcurrency = 1;
  override readonly maxIndexConcurrency = 1;

  private readonly modelCacheDir: string;
  private readonly executionProvider: TransformersJsExecutionProvider | null;
  private pipeline: FeatureExtractionPipeline | null = null;
  private pipelineLoadPromise: Promise<FeatureExtractionPipeline> | null = null;
  private usingCpuFallback = false;
  private disposed = false;

  constructor(
    private readonly entry: TransformersJsEmbeddingCatalogEntry,
    options: ModelProviderOptions,
  ) {
    super();
    this.ref = { provider: entry.provider, model: entry.model } as const;
    this.dimension = entry.dimension;
    this.metric = entry.metric;
    this.limits = {
      maxBatchSize: entry.maxBatchSize,
      maxInputTokens: entry.maxInputTokens,
    } as const satisfies EmbeddingLimits;
    this.modelCacheDir =
      options.modelCacheDir ??
      process.env.ZVEC_GREP_MODEL_CACHE ??
      DEFAULT_MODEL_CACHE_DIR;
    this.executionProvider = resolveExecutionProvider(options.llamaGpu);
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: Required<EmbeddingOptions>,
  ): Promise<EmbeddingVector[]> {
    return (await this.embedBatch(contents, options)).vectors;
  }

  protected override async doEmbedWithDiagnostics(
    contents: readonly Content[],
    options: Required<EmbeddingOptions>,
  ): Promise<EmbeddingBatchResult> {
    return await this.embedBatch(contents, options);
  }

  private async embedBatch(
    contents: readonly Content[],
    options: Required<EmbeddingOptions>,
  ): Promise<EmbeddingBatchResult> {
    this.ensureNotDisposed();
    const texts = (contents as readonly TextContent[]).map((content) =>
      formatText(content.text, options.purpose, this.entry),
    );
    let pipeline: FeatureExtractionPipeline;
    try {
      pipeline = await this.ensurePipeline();
    } catch (cause) {
      throw new EngineError("Transformers.js embedding failed", {
        code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_EMBED_FAILED",
        context: `model=${this.entry.id} repo=${this.entry.repo}`,
        cause,
      });
    }
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
        context: `model=${this.entry.id} repo=${this.entry.repo}`,
        cause,
      });
    }

    let failure: unknown;
    try {
      return await this.embedTexts(pipeline, texts, truncatedInputIndexes);
    } catch (cause) {
      failure = cause;
    }

    if (await this.fallbackToCpu(failure)) {
      try {
        return await this.embedTexts(
          await this.ensurePipeline(),
          texts,
          truncatedInputIndexes,
        );
      } catch (cause) {
        failure = cause;
      }
    }

    throw new EngineError("Transformers.js embedding failed", {
      code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_EMBED_FAILED",
      context: `model=${this.entry.id} repo=${this.entry.repo}`,
      cause: failure,
    });
  }

  override async dispose(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    const pipeline = this.pipeline;
    this.pipeline = null;
    this.pipelineLoadPromise = null;
    await pipeline?.dispose();
  }

  private async ensurePipeline(): Promise<FeatureExtractionPipeline> {
    if (this.pipeline) {
      return this.pipeline;
    }
    if (this.pipelineLoadPromise) {
      return await this.pipelineLoadPromise;
    }

    this.pipelineLoadPromise = this.loadPipeline();
    try {
      this.pipeline = await this.pipelineLoadPromise;
      return this.pipeline;
    } finally {
      this.pipelineLoadPromise = null;
    }
  }

  private async loadPipeline(): Promise<FeatureExtractionPipeline> {
    const runtime = await loadTransformersJs();
    let pipeline: FeatureExtractionPipeline;
    const executionProvider = this.usingCpuFallback
      ? "cpu"
      : this.executionProvider;

    try {
      pipeline = await this.createPipeline(runtime, executionProvider);
    } catch (cause) {
      if (!executionProvider || executionProvider === "cpu") {
        throw cause;
      }

      process.stderr.write(
        `zvec-grep warning: Transformers.js ${executionProvider} embedding initialization failed (${formatErrorMessage(cause)}), falling back to CPU.\n`,
      );
      this.usingCpuFallback = true;
      pipeline = await this.createPipeline(runtime, "cpu");
    }

    pipeline.tokenizer.model_max_length = this.entry.maxInputTokens;
    return pipeline;
  }

  private async embedTexts(
    pipeline: FeatureExtractionPipeline,
    texts: string[],
    truncatedInputIndexes: number[],
  ): Promise<EmbeddingBatchResult> {
    const tensor = await pipeline(texts, {
      pooling: this.entry.pooling,
      normalize: this.entry.normalize,
      truncation: true,
      max_length: this.entry.maxInputTokens,
    });
    return {
      vectors: tensorToVectors(tensor, texts.length, this.entry.dimension),
      diagnostics: {
        truncatedInputIndexes,
      },
    };
  }

  private async fallbackToCpu(cause: unknown): Promise<boolean> {
    if (
      this.usingCpuFallback ||
      !this.executionProvider ||
      this.executionProvider === "cpu"
    ) {
      return false;
    }

    process.stderr.write(
      `zvec-grep warning: Transformers.js ${this.executionProvider} embedding inference failed (${formatErrorMessage(cause)}), falling back to CPU.\n`,
    );
    this.usingCpuFallback = true;
    const pipeline = this.pipeline;
    this.pipeline = null;
    this.pipelineLoadPromise = null;
    await pipeline?.dispose();
    return true;
  }

  private async createPipeline(
    runtime: TransformersJsModule,
    executionProvider: TransformersJsExecutionProvider | null,
  ): Promise<FeatureExtractionPipeline> {
    return await runtime.pipeline("feature-extraction", this.entry.repo, {
      cache_dir: this.modelCacheDir,
      revision: this.entry.revision,
      dtype: this.entry.dtype,
      ...(executionProvider
        ? { session_options: { executionProviders: [executionProvider] } }
        : {}),
    });
  }

  private ensureNotDisposed(): void {
    if (this.disposed) {
      throw new EngineError("Transformers.js embedding model is disposed", {
        code: "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_DISPOSED",
        context: `model=${this.entry.id}`,
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
  gpu: ModelProviderOptions["llamaGpu"],
): TransformersJsExecutionProvider | null {
  if (gpu === undefined) {
    return null;
  }
  if (gpu === false) {
    return "cpu";
  }
  if (gpu === "metal" || gpu === "vulkan") {
    return "webgpu";
  }
  if (gpu === "cuda") {
    return "cuda";
  }

  if (process.platform === "win32") {
    return "dml";
  }
  if (process.platform === "linux" && process.arch === "x64") {
    return "cuda";
  }
  return "webgpu";
}

function formatErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatText(
  text: string,
  purpose: Required<EmbeddingOptions>["purpose"],
  entry: TransformersJsEmbeddingCatalogEntry,
): string {
  const prefix = purpose === "query" ? entry.queryPrefix : entry.documentPrefix;
  return prefix ? `${prefix}${text}` : text;
}

function tensorToVectors(
  tensor: TensorLike,
  count: number,
  dimension: number,
): EmbeddingVector[] {
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
