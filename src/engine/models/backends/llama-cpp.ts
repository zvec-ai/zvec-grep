import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readSync,
  statSync,
  unlinkSync,
} from "node:fs";
import { join } from "node:path";
import { EngineError } from "../../errors/index.js";
import type { Content, TextContent } from "../../types.js";
import { defaultHome } from "../../utils/path.js";
import {
  BaseEmbeddingModel,
  type CreateEmbeddingModelOptions,
  type EmbeddingModelInfo,
  type EmbeddingResult,
  type NormalizedEmbeddingOptions,
} from "../embeddings.js";
import type { LlamaCppEmbeddingCatalogEntry } from "../catalog.js";

type LlamaEmbedding = {
  vector: ArrayLike<number>;
};

type LlamaEmbeddingContext = {
  getEmbeddingFor(text: string): Promise<LlamaEmbedding>;
  dispose?(): Promise<void> | void;
};

type LlamaModel = {
  trainContextSize?: number;
  tokenize?(text: string): readonly unknown[];
  detokenize?(tokens: readonly unknown[]): string;
  createEmbeddingContext(
    options: Record<string, unknown>,
  ): Promise<LlamaEmbeddingContext>;
  dispose?(): Promise<void> | void;
};

type Llama = {
  gpu: string | false;
  cpuMathCores?: number;
  supportsGpuOffloading?: boolean;
  getVramState?(): Promise<{ total: number; used: number; free: number }>;
  loadModel(options: {
    modelPath: string;
    gpuLayers?: number;
  }): Promise<LlamaModel>;
  dispose?(): Promise<void> | void;
};

type LlamaGpuSelection =
  Exclude<NonNullable<CreateEmbeddingModelOptions["device"]>, "cpu"> | false;

type NodeLlamaCppModule = {
  getLlama(options: Record<string, unknown>): Promise<Llama>;
  resolveModelFile(
    model: string,
    options: { directory: string; cli?: boolean },
  ): Promise<string>;
  LlamaLogLevel?: { error?: unknown };
};

type NodeLlamaCppLoader = () => Promise<NodeLlamaCppModule>;
type LlamaCppRuntimeState = {
  failedGpuInitModes: Set<LlamaGpuSelection>;
  cpuCompatibleFallbackWarningShown: boolean;
};
type LlamaCppDependencies = {
  loadRuntime: NodeLlamaCppLoader;
  runtimeState: LlamaCppRuntimeState;
};

const DEFAULT_MODEL_CACHE_DIR = join(defaultHome(), "models");
const GGUF_MAGIC = Buffer.from("GGUF");
const DEFAULT_PARALLELISM_CAP = 8;
const DEFAULT_DARWIN_CMAKE_OPTIONS = {
  GGML_OPENMP: "OFF",
} as const;
const DEFAULT_DARWIN_ARM64_CMAKE_OPTIONS = {
  ...DEFAULT_DARWIN_CMAKE_OPTIONS,
  GGML_NATIVE: "OFF",
} as const;
const SUPPRESSED_LLAMA_CPP_LOG_MESSAGES = new Set(["Failed to get swap info"]);

async function defaultNodeLlamaCppLoader(): Promise<NodeLlamaCppModule> {
  const moduleName = "node-llama-cpp";
  try {
    installDarwinMetalResidencyMitigation();
    return (await import(moduleName)) as NodeLlamaCppModule;
  } catch (cause) {
    throw new EngineError(
      "node-llama-cpp is required for local embedding models",
      {
        code: "ZVEC_GREP.ENGINE.MODELS.LLAMA_CPP_MISSING_DEPENDENCY",
        context:
          "Install optional dependency node-llama-cpp or reinstall zvec-grep with optional dependencies enabled",
        cause,
      },
    );
  }
}

function installDarwinMetalResidencyMitigation(): void {
  if (process.platform !== "darwin") {
    return;
  }

  if (process.env.ZVEC_GREP_METAL_KEEP_RESIDENCY === "1") {
    return;
  }

  process.env.GGML_METAL_NO_RESIDENCY ??= "1";
}

let defaultRuntimeImport: Promise<NodeLlamaCppModule> | null = null;

const defaultDependencies: LlamaCppDependencies = {
  loadRuntime() {
    defaultRuntimeImport ??= defaultNodeLlamaCppLoader();
    return defaultRuntimeImport;
  },
  runtimeState: {
    failedGpuInitModes: new Set<LlamaGpuSelection>(),
    cpuCompatibleFallbackWarningShown: false,
  },
};

export class LlamaCppEmbeddingModel extends BaseEmbeddingModel {
  readonly info: EmbeddingModelInfo;

  private readonly modelCacheDir: string;
  private readonly gpu: LlamaGpuSelection;
  private readonly parallelism?: number;
  private readonly dependencies: LlamaCppDependencies;

  private runtimeImport: Promise<NodeLlamaCppModule> | null = null;
  private llama: Llama | null = null;
  private model: LlamaModel | null = null;
  private contexts: LlamaEmbeddingContext[] = [];
  private llamaLoadPromise: Promise<Llama> | null = null;
  private modelLoadPromise: Promise<LlamaModel> | null = null;
  private contextsCreatePromise: Promise<LlamaEmbeddingContext[]> | null = null;
  private usingCpuFallback = false;
  private disposed = false;

  constructor(
    private readonly entry: LlamaCppEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
    dependencies: Partial<LlamaCppDependencies> = {},
  ) {
    super();

    this.info = {
      reference: entry.reference,
      provider: entry.provider,
      name: entry.model,
      dimension: entry.dimension,
      metric: entry.metric,
      inputKinds: ["text"],
      limits: {
        maxBatchSize: entry.maxBatchSize,
        maxInputTokens: entry.contextSize,
      },
    };
    this.modelCacheDir =
      options.modelCacheDir ??
      process.env.ZVEC_GREP_MODEL_CACHE ??
      DEFAULT_MODEL_CACHE_DIR;
    this.gpu = embeddingDeviceToLlamaGpuSelection(options.device ?? "cpu");
    this.parallelism = resolveParallelismOverride(
      process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM,
    );
    this.dependencies = { ...defaultDependencies, ...dependencies };
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
    this.ensureNotDisposed();
    const texts = (contents as readonly TextContent[]).map((content) =>
      formatTextForEmbedding(content.text, options.purpose, this.entry),
    );

    try {
      return await this.embedTexts(texts);
    } catch (cause) {
      throw new EngineError("llama.cpp embedding failed", {
        code: "ZVEC_GREP.ENGINE.MODELS.LLAMA_CPP_EMBED_FAILED",
        context: `model=${this.entry.reference}`,
        cause,
      });
    }
  }

  override async dispose(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.disposed = true;

    await this.disposeLoadedRuntime();
    this.modelLoadPromise = null;
    this.contextsCreatePromise = null;
  }

  private async embedTexts(texts: readonly string[]): Promise<EmbeddingResult> {
    const contexts = await this.ensureEmbeddingContexts(texts.length);
    const truncatedInputIndexes: number[] = [];
    const safeTexts = texts.map((text, index) => {
      const result = this.truncateToContextSize(text);
      if (result.truncated) {
        truncatedInputIndexes.push(index);
      }
      return result.text;
    });
    const chunkSize = Math.ceil(texts.length / contexts.length);
    const chunks = contexts
      .map((context, index) => ({
        context,
        texts: safeTexts.slice(index * chunkSize, (index + 1) * chunkSize),
      }))
      .filter((chunk) => chunk.texts.length > 0);

    const results = await Promise.all(
      chunks.map(async (chunk) => {
        const vectors: number[][] = [];
        for (const text of chunk.texts) {
          const embedding = await chunk.context.getEmbeddingFor(text);
          vectors.push(Array.from(embedding.vector));
        }
        return vectors;
      }),
    );

    return {
      vectors: results.flat(),
      truncated: truncatedInputIndexes,
    };
  }

  private async ensureLlama(): Promise<Llama> {
    if (this.llama) {
      return this.llama;
    }

    if (this.llamaLoadPromise) {
      return await this.llamaLoadPromise;
    }

    this.llamaLoadPromise = this.loadLlamaWithFallback();

    try {
      return await this.llamaLoadPromise;
    } finally {
      this.llamaLoadPromise = null;
    }
  }

  private async loadRuntime(): Promise<NodeLlamaCppModule> {
    this.runtimeImport ??= this.dependencies.loadRuntime();
    return await this.runtimeImport;
  }

  private async loadLlamaWithFallback(): Promise<Llama> {
    const runtime = await this.loadRuntime();
    const requestedGpu = this.usingCpuFallback ? false : this.gpu;
    const load = (gpu: LlamaGpuSelection) =>
      runtime.getLlama({
        build: "autoAttempt",
        logLevel: runtime.LlamaLogLevel?.error,
        logger: llamaCppLogger,
        gpu,
        cmakeOptions: defaultLlamaCppCmakeOptions(),
        progressLogs: false,
      });

    if (requestedGpu === false) {
      this.llama = await this.loadCpuCompatibleLlama(load);
      return this.llama;
    }

    if (this.dependencies.runtimeState.failedGpuInitModes.has(requestedGpu)) {
      process.stderr.write(
        `zvec-grep warning: skipping previously failed llama.cpp GPU init${requestedGpu === "auto" ? "" : ` for device=${requestedGpu}`}, using CPU.\n`,
      );
      this.usingCpuFallback = true;
      this.llama = await this.loadCpuCompatibleLlama(load);
      return this.llama;
    }

    try {
      this.llama = await load(requestedGpu);
    } catch (error) {
      this.dependencies.runtimeState.failedGpuInitModes.add(requestedGpu);
      process.stderr.write(
        `zvec-grep warning: llama.cpp GPU init failed (${formatErrorMessage(error)}), falling back to CPU.\n`,
      );
      this.usingCpuFallback = true;
      this.llama = await this.loadCpuCompatibleLlama(load);
    }

    return this.llama;
  }

  private async loadCpuCompatibleLlama(
    load: (gpu: LlamaGpuSelection) => Promise<Llama>,
  ): Promise<Llama> {
    try {
      return await load(false);
    } catch (error) {
      if (!this.dependencies.runtimeState.cpuCompatibleFallbackWarningShown) {
        this.dependencies.runtimeState.cpuCompatibleFallbackWarningShown = true;
        process.stderr.write(
          `zvec-grep warning: CPU-only llama.cpp backend unavailable (${formatErrorMessage(error)}); using packaged backend with GPU model offloading disabled.\n`,
        );
      }
      return await load("auto");
    }
  }

  private async ensureModel(): Promise<LlamaModel> {
    if (this.model) {
      return this.model;
    }

    if (this.modelLoadPromise) {
      return await this.modelLoadPromise;
    }

    this.modelLoadPromise = this.loadModelWithFallback();

    try {
      return await this.modelLoadPromise;
    } finally {
      this.modelLoadPromise = null;
    }
  }

  private async loadModelWithFallback(): Promise<LlamaModel> {
    try {
      return await this.loadModel();
    } catch (error) {
      if (!this.canRetryGpuOperationOnCpu()) {
        throw error;
      }

      process.stderr.write(
        `zvec-grep warning: llama.cpp GPU model load failed (${formatErrorMessage(error)}), falling back to CPU.\n`,
      );
      this.usingCpuFallback = true;
      await this.disposeLoadedRuntime();
      return await this.loadModel();
    }
  }

  private async loadModel(): Promise<LlamaModel> {
    const llama = await this.ensureLlama();
    const modelPath = await this.resolveModelPath();
    const model = await llama.loadModel(this.modelLoadOptions(modelPath));
    this.model = model;
    return model;
  }

  private modelLoadOptions(modelPath: string): {
    modelPath: string;
    gpuLayers?: number;
  } {
    return {
      modelPath,
      ...(this.shouldDisableModelGpuOffload() ? { gpuLayers: 0 } : {}),
    };
  }

  private async ensureEmbeddingContexts(
    textCount: number,
  ): Promise<LlamaEmbeddingContext[]> {
    const targetParallelism = await this.resolveEffectiveParallelism(textCount);
    if (this.contexts.length >= targetParallelism) {
      return this.contexts.slice(0, targetParallelism);
    }

    if (this.contextsCreatePromise) {
      await this.contextsCreatePromise;
      if (this.contexts.length >= targetParallelism) {
        return this.contexts.slice(0, targetParallelism);
      }
    }

    this.contextsCreatePromise =
      this.createEmbeddingContextsWithFallback(targetParallelism);

    try {
      await this.contextsCreatePromise;
      return this.contexts.slice(0, targetParallelism);
    } finally {
      this.contextsCreatePromise = null;
    }
  }

  private async createEmbeddingContextsWithFallback(
    targetParallelism: number,
  ): Promise<LlamaEmbeddingContext[]> {
    try {
      return await this.createEmbeddingContexts(targetParallelism);
    } catch (error) {
      if (!this.canRetryGpuOperationOnCpu()) {
        throw error;
      }

      process.stderr.write(
        `zvec-grep warning: llama.cpp GPU embedding context failed (${formatErrorMessage(error)}), falling back to CPU.\n`,
      );
      this.usingCpuFallback = true;
      await this.disposeLoadedRuntime();
      return await this.createEmbeddingContexts(targetParallelism);
    }
  }

  private async createEmbeddingContexts(
    targetParallelism: number,
  ): Promise<LlamaEmbeddingContext[]> {
    const model = await this.ensureModel();
    const threads = await this.resolveThreadsPerContext(targetParallelism);
    const initialContextCount = this.contexts.length;

    while (this.contexts.length < targetParallelism) {
      try {
        this.contexts.push(
          await model.createEmbeddingContext({
            contextSize: this.entry.contextSize,
            threads,
          }),
        );
      } catch (error) {
        if (this.contexts.length === initialContextCount) {
          throw error;
        }
        break;
      }
    }

    return this.contexts.slice(0, targetParallelism);
  }

  private canRetryGpuOperationOnCpu(): boolean {
    return this.gpu !== false && !this.usingCpuFallback;
  }

  private shouldDisableModelGpuOffload(): boolean {
    return this.gpu === false || this.usingCpuFallback;
  }

  private async disposeLoadedRuntime(): Promise<void> {
    const contexts = this.contexts;
    this.contexts = [];
    for (const context of contexts) {
      await context.dispose?.();
    }

    const model = this.model;
    this.model = null;
    await model?.dispose?.();

    const llama = this.llama;
    this.llama = null;
    if (llama?.dispose) {
      await Promise.race([
        Promise.resolve(llama.dispose()),
        new Promise<void>((resolve) => setTimeout(resolve, 1000)),
      ]);
    }

    this.modelLoadPromise = null;
    this.llamaLoadPromise = null;
  }

  private async resolveModelPath(): Promise<string> {
    mkdirSync(this.modelCacheDir, { recursive: true });

    const runtime = await this.loadRuntime();
    const modelPath = await runtime.resolveModelFile(this.entry.uri, {
      directory: this.modelCacheDir,
      cli: false,
    });
    validateGgufFile(modelPath, this.entry.uri);
    return modelPath;
  }

  private async resolveParallelism(): Promise<number> {
    if (this.parallelism !== undefined) {
      return this.parallelism;
    }

    const llama = await this.ensureLlama();
    if (
      !this.shouldDisableModelGpuOffload() &&
      llama.gpu &&
      llama.getVramState
    ) {
      try {
        const vram = await llama.getVramState();
        const freeMb = vram.free / (1024 * 1024);
        return Math.max(
          1,
          Math.min(DEFAULT_PARALLELISM_CAP, Math.floor((freeMb * 0.25) / 150)),
        );
      } catch {
        return 2;
      }
    }

    return 1;
  }

  private async resolveEffectiveParallelism(
    textCount: number,
  ): Promise<number> {
    const requested = await this.resolveParallelism();
    return Math.max(1, Math.min(requested, Math.max(1, textCount)));
  }

  private async resolveThreadsPerContext(parallelism: number): Promise<number> {
    const llama = await this.ensureLlama();
    if (!this.shouldDisableModelGpuOffload() && llama.gpu) {
      return 0;
    }

    const cores = llama.cpuMathCores ?? 4;
    if (parallelism <= 1) {
      return 0;
    }

    return Math.max(1, Math.floor(cores / parallelism));
  }

  private truncateToContextSize(text: string): {
    text: string;
    truncated: boolean;
  } {
    const model = this.model;
    if (!model?.tokenize || !model.detokenize) {
      return { text, truncated: false };
    }

    const limit = Math.max(
      1,
      Math.min(
        this.entry.contextSize,
        model.trainContextSize ?? this.entry.contextSize,
      ),
    );
    const tokens = model.tokenize(text);
    if (tokens.length <= limit) {
      return { text, truncated: false };
    }

    return {
      text: model.detokenize(tokens.slice(0, Math.max(1, limit - 4))),
      truncated: true,
    };
  }

  private ensureNotDisposed(): void {
    if (this.disposed) {
      throw new EngineError("llama.cpp embedding model is disposed", {
        code: "ZVEC_GREP.ENGINE.MODELS.LLAMA_CPP_DISPOSED",
        context: `model=${this.entry.reference}`,
      });
    }
  }
}

function formatErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function llamaCppLogger(_level: unknown, message: string): void {
  const trimmed = message.trim();
  if (SUPPRESSED_LLAMA_CPP_LOG_MESSAGES.has(trimmed)) {
    return;
  }

  process.stderr.write(formatLlamaCppLogMessage(message));
}

function formatLlamaCppLogMessage(message: string): string {
  const text = message.trimEnd();
  if (!text) {
    return "";
  }

  return `${text
    .split("\n")
    .map((line) => `[node-llama-cpp] ${line}`)
    .join("\n")}\n`;
}

function defaultLlamaCppCmakeOptions(): Record<string, string> | undefined {
  if (process.platform !== "darwin") {
    return undefined;
  }

  // macOS machines commonly lack libomp, and Apple Clang's native ARM flag
  // probe emits a scary CMake warning despite falling back successfully.
  if (process.arch === "arm64") {
    return { ...DEFAULT_DARWIN_ARM64_CMAKE_OPTIONS };
  }

  return { ...DEFAULT_DARWIN_CMAKE_OPTIONS };
}

function formatTextForEmbedding(
  text: string,
  purpose: NormalizedEmbeddingOptions["purpose"],
  entry: LlamaCppEmbeddingCatalogEntry,
): string {
  if (entry.format === "qwen3") {
    return purpose === "query"
      ? `Instruct: Retrieve relevant documents for the given query\nQuery: ${text}`
      : text;
  }

  return purpose === "query"
    ? `task: search result | query: ${text}`
    : `title: none | text: ${text}`;
}

function embeddingDeviceToLlamaGpuSelection(
  device: NonNullable<CreateEmbeddingModelOptions["device"]>,
): LlamaGpuSelection {
  return device === "cpu" ? false : device;
}

function resolveParallelismOverride(
  envValue: string | undefined,
): number | undefined {
  const normalized = envValue?.trim() ?? "";
  if (!normalized) {
    return undefined;
  }

  const parsed = Number.parseInt(normalized, 10);
  if (!Number.isInteger(parsed) || parsed < 1) {
    process.stderr.write(
      `zvec-grep warning: invalid ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM="${envValue}", using automatic parallelism.\n`,
    );
    return undefined;
  }

  return Math.min(DEFAULT_PARALLELISM_CAP, parsed);
}

function validateGgufFile(filePath: string, modelUri: string): void {
  if (!existsSync(filePath)) {
    return;
  }

  const fd = openSync(filePath, "r");
  const sniff = Buffer.alloc(512);
  try {
    readSync(fd, sniff, 0, 512, 0);
  } finally {
    closeSync(fd);
  }

  const header = sniff.subarray(0, 4);
  if (header.equals(GGUF_MAGIC)) {
    return;
  }

  const text = sniff.toString("utf8").toLowerCase();
  const isHtml = text.includes("<!doctype") || text.includes("<html");
  const got = header.toString("utf8");
  const sizeKb = existsSync(filePath)
    ? (statSync(filePath).size / 1024).toFixed(0)
    : "0";

  unlinkSync(filePath);

  if (isHtml) {
    throw new EngineError(
      "Downloaded local embedding model is HTML, not GGUF",
      {
        code: "ZVEC_GREP.ENGINE.MODELS.LLAMA_CPP_INVALID_GGUF_HTML",
        context: `model=${modelUri} path=${filePath} sizeKB=${sizeKb}`,
      },
    );
  }

  throw new EngineError("Local embedding model is not a valid GGUF file", {
    code: "ZVEC_GREP.ENGINE.MODELS.LLAMA_CPP_INVALID_GGUF",
    context: `model=${modelUri} path=${filePath} expected=GGUF actual=${got} sizeKB=${sizeKb}`,
  });
}
