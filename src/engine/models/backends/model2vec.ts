import { open, readFile } from "node:fs/promises";
import type { FileHandle } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { EngineError } from "../../errors.js";
import type { Content, TextContent } from "../../types.js";
import { writeJsonFile } from "../../utils/json.js";
import { defaultHome } from "../../utils/path.js";
import {
  BaseEmbeddingModel,
  type CreateEmbeddingModelOptions,
  type EmbeddingModelProgress,
  type EmbeddingModelInfo,
  type EmbeddingOptions,
  type EmbeddingResult,
  type NormalizedEmbeddingOptions,
} from "../embeddings.js";
import type { Model2VecEmbeddingCatalogEntry } from "../catalog.js";
import {
  resolveModelArtifacts,
  type ModelArtifactSource,
} from "../artifact-downloader.js";
import { createModelDownloadProgressReporter } from "../download-progress.js";
import { loadModel2VecTokenizer } from "./model2vec-tokenizer.js";
import { Model2VecWorkerPool } from "./model2vec-worker-pool.js";
import {
  embedModel2VecTexts,
  sharedStaticEmbeddingTable,
  type StaticEmbeddingTable,
  type TokenizerLike,
} from "./model2vec-runtime.js";

type Model2VecDependencies = {
  loadTokenizer(
    repo: string,
    options: {
      cache_dir: string;
      revision: string;
      local_files_only?: boolean;
    },
  ): Promise<TokenizerLike>;
  loadSafetensors(
    path: string,
    tensorName: string,
    dimension: number,
  ): Promise<StaticEmbeddingTable>;
  resolveArtifacts: typeof resolveModelArtifacts;
};

const DEFAULT_MODEL_CACHE_DIR = join(defaultHome(), "models");

const defaultDependencies: Model2VecDependencies = {
  async loadTokenizer(source) {
    return await loadModel2VecTokenizer(source);
  },
  async loadSafetensors(path, tensorName, dimension) {
    return await readStaticEmbeddingTable(path, tensorName, dimension);
  },
  resolveArtifacts: resolveModelArtifacts,
};

export class Model2VecEmbeddingModel extends BaseEmbeddingModel {
  readonly info: EmbeddingModelInfo;

  private readonly modelCacheDir: string;
  private readonly dependencies: Model2VecDependencies;
  private tokenizer: TokenizerLike | null = null;
  private staticTable: StaticEmbeddingTable | null = null;
  private workerPool: Model2VecWorkerPool | null = null;
  private loadPromise: Promise<void> | null = null;
  private readonly useWorkerPool: boolean;
  private sourceFallbackWarningReported = false;
  private disposed = false;

  constructor(
    private readonly entry: Model2VecEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
    dependencies: Partial<Model2VecDependencies> = {},
  ) {
    super();
    this.info = {
      reference: entry.reference,
      provider: entry.provider,
      name: entry.model,
      dimension: entry.dimension,
      metric: entry.metric,
      defaultConcurrency: entry.defaultConcurrency,
      inputKinds: ["text"],
      limits: {
        maxBatchSize: entry.maxBatchSize,
        maxInputTokens: entry.maxInputTokens,
      },
    };
    this.modelCacheDir = resolve(
      options.modelCacheDir ??
        process.env.ZVEC_GREP_MODEL_CACHE ??
        DEFAULT_MODEL_CACHE_DIR,
    );
    this.dependencies = { ...defaultDependencies, ...dependencies };
    this.useWorkerPool = Object.keys(dependencies).length === 0;
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult> {
    return await this.embedBatch(contents, options);
  }

  async prepare(
    options: Pick<EmbeddingOptions, "signal" | "onProgress"> = {},
  ): Promise<void> {
    options.signal?.throwIfAborted();
    this.ensureNotDisposed();
    await this.ensureLoaded(options.onProgress);
    options.signal?.throwIfAborted();
    this.ensureNotDisposed();
  }

  private async embedBatch(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult> {
    this.ensureNotDisposed();
    await this.ensureLoaded(options.onProgress);

    try {
      const texts = (contents as readonly TextContent[]).map((content) =>
        formatText(content.text, options.purpose, this.entry),
      );
      if (this.workerPool) {
        return await this.workerPool.run(texts, options.signal);
      }
      return await this.embedTexts(texts);
    } catch (cause) {
      throw new EngineError("Model2Vec embedding failed", {
        code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_EMBED_FAILED",
        context: `model=${this.entry.reference} repo=${this.entry.repo}`,
        cause,
      });
    }
  }

  override async dispose(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    await this.workerPool?.dispose();
    this.workerPool = null;
    this.staticTable = null;
    this.tokenizer = null;
    this.loadPromise = null;
  }

  private async ensureLoaded(
    onProgress?: (progress: EmbeddingModelProgress) => void,
  ): Promise<void> {
    if (this.workerPool || (this.tokenizer && this.staticTable)) {
      return;
    }
    if (this.loadPromise) {
      return await this.loadPromise;
    }

    this.loadPromise = this.loadModel(onProgress);
    try {
      await this.loadPromise;
    } finally {
      this.loadPromise = null;
    }
  }

  private async loadModel(
    onProgress?: (progress: EmbeddingModelProgress) => void,
  ): Promise<void> {
    const downloadProgress = createModelDownloadProgressReporter(
      this.entry.reference,
      onProgress,
    );
    downloadProgress.start();
    try {
      const resolved = await this.dependencies
        .resolveArtifacts({
          model: this.entry.reference,
          sources: this.createArtifactSources(),
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
        })
        .catch((cause: unknown) => {
          throw new EngineError(
            "Unable to download Model2Vec model artifacts",
            {
              code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED",
              context: `model=${this.entry.reference} repo=${this.entry.repo} revision=${this.entry.revision}`,
              cause,
            },
          );
        });
      const modelPath = resolved.paths[this.entry.modelFile];
      const tokenizerPath = resolved.paths[this.entry.tokenizerFile];
      if (!modelPath || !tokenizerPath) {
        throw new EngineError("Resolved Model2Vec snapshot is incomplete", {
          code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED",
          context: `model=${this.entry.reference} source=${resolved.source.kind}`,
        });
      }
      const tokenizerSource = dirname(tokenizerPath);
      const configPath = join(tokenizerSource, "tokenizer_config.json");
      await ensureTokenizerConfig(configPath);
      const staticTable = await this.dependencies.loadSafetensors(
        modelPath,
        this.entry.embeddingTensor,
        this.info.dimension,
      );
      this.ensureNotDisposed();
      if (this.useWorkerPool) {
        const sharedTable = sharedStaticEmbeddingTable(staticTable);
        const workerPool = new Model2VecWorkerPool({
          tokenizerSource,
          maxInputTokens: this.entry.maxInputTokens,
          normalize: this.entry.normalize,
          tableBuffer: sharedTable.data.buffer as SharedArrayBuffer,
          dimension: sharedTable.dimension,
          dtype: sharedTable.dtype,
          rows: sharedTable.rows,
        });
        try {
          await workerPool.start();
          this.ensureNotDisposed();
          this.workerPool = workerPool;
        } catch (error) {
          await workerPool.dispose();
          throw error;
        }
      } else {
        this.tokenizer = await this.dependencies.loadTokenizer(
          tokenizerSource,
          {
            cache_dir: this.modelCacheDir,
            revision: resolved.source.revision,
            local_files_only: true,
          },
        );
        this.ensureNotDisposed();
        this.staticTable = staticTable;
      }
      downloadProgress.finish();
    } catch (cause) {
      downloadProgress.warning(
        "Unable to prepare the local embedding model. Check network access and the model cache.",
      );
      if (
        cause instanceof EngineError &&
        (cause.code === "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED" ||
          cause.code === "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DISPOSED")
      ) {
        throw cause;
      }
      throw new EngineError("Unable to load Model2Vec model", {
        code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_LOAD_FAILED",
        context: `model=${this.entry.reference} repo=${this.entry.repo} revision=${this.entry.revision}`,
        cause,
      });
    }
  }

  private createArtifactSources(): readonly ModelArtifactSource[] {
    const huggingFace = this.entry.sources.huggingFace;
    const modelScope = this.entry.sources.modelScope;
    const localPaths = {
      [this.entry.modelFile]: basename(this.entry.modelFile),
      [this.entry.tokenizerFile]: "tokenizer/tokenizer.json",
    };
    return [
      {
        kind: "huggingface",
        repo: huggingFace.repo,
        revision: huggingFace.revision,
        cacheDirectory: join(
          this.modelCacheDir,
          "model2vec",
          huggingFace.repo.replaceAll("/", "--"),
          huggingFace.revision,
        ),
        localPaths,
      },
      {
        kind: "modelscope",
        repo: modelScope.repo,
        revision: modelScope.revision,
        cacheDirectory: join(
          this.modelCacheDir,
          "modelscope",
          "model2vec",
          modelScope.repo.replaceAll("/", "--"),
          modelScope.revision,
        ),
        localPaths,
      },
    ];
  }

  private async embedTexts(texts: string[]): Promise<EmbeddingResult> {
    const tokenizer = this.tokenizer;
    const staticTable = this.staticTable;
    if (!tokenizer || !staticTable) {
      throw new Error("Model2Vec model is not loaded");
    }

    return await embedModel2VecTexts(
      texts,
      tokenizer,
      staticTable,
      this.entry.maxInputTokens,
      this.entry.normalize,
    );
  }

  private ensureNotDisposed(): void {
    if (this.disposed) {
      throw new EngineError("Model2Vec embedding model is disposed", {
        code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DISPOSED",
        context: `model=${this.entry.reference}`,
      });
    }
  }
}

async function ensureTokenizerConfig(path: string): Promise<void> {
  try {
    const value = JSON.parse(await readFile(path, "utf8")) as unknown;
    if (
      typeof value === "object" &&
      value !== null &&
      "tokenizer_class" in value &&
      value.tokenizer_class === "PreTrainedTokenizer"
    ) {
      return;
    }
  } catch (error) {
    if (
      !(error instanceof SyntaxError) &&
      !(
        typeof error === "object" &&
        error !== null &&
        "code" in error &&
        error.code === "ENOENT"
      )
    ) {
      throw error;
    }
  }

  await writeJsonFile(path, { tokenizer_class: "PreTrainedTokenizer" });
}

function formatText(
  text: string,
  purpose: NormalizedEmbeddingOptions["purpose"],
  entry: Model2VecEmbeddingCatalogEntry,
): string {
  const prefix =
    purpose === "query"
      ? "queryPrefix" in entry && typeof entry.queryPrefix === "string"
        ? entry.queryPrefix
        : undefined
      : "documentPrefix" in entry && typeof entry.documentPrefix === "string"
        ? entry.documentPrefix
        : undefined;
  return prefix ? `${prefix}${text}` : text;
}

async function readStaticEmbeddingTable(
  path: string,
  tensorName: string,
  expectedDimension: number,
): Promise<StaticEmbeddingTable> {
  const file = await open(path, "r");
  try {
    const stats = await file.stat();
    if (stats.size < 9) {
      throw new Error("Safetensors file is too small");
    }

    const prefix = Buffer.alloc(8);
    await readExactly(file, prefix, 0);
    const headerLength = Number(prefix.readBigUInt64LE(0));
    const dataStart = 8 + headerLength;
    if (!Number.isSafeInteger(headerLength) || dataStart > stats.size) {
      throw new Error("Safetensors header length is invalid");
    }

    const headerBytes = Buffer.alloc(headerLength);
    await readExactly(file, headerBytes, 8);
    let header: Record<
      string,
      { data_offsets?: [number, number]; dtype?: string; shape?: number[] }
    >;
    try {
      header = JSON.parse(headerBytes.toString("utf8")) as typeof header;
    } catch (cause) {
      throw new Error("Safetensors header is invalid JSON", { cause });
    }
    const tensor = header[tensorName];
    if (
      !tensor ||
      tensor.shape?.length !== 2 ||
      tensor.shape[1] !== expectedDimension ||
      tensor.data_offsets?.length !== 2 ||
      (tensor.dtype !== "F16" && tensor.dtype !== "F32")
    ) {
      throw new Error(
        `Safetensors tensor '${tensorName}' is missing or incompatible`,
      );
    }

    const [relativeStart, relativeEnd] = tensor.data_offsets;
    const rows = tensor.shape[0];
    const valueCount = rows * expectedDimension;
    const bytesPerValue = tensor.dtype === "F16" ? 2 : 4;
    const tensorByteLength = valueCount * bytesPerValue;
    if (
      !Number.isSafeInteger(valueCount) ||
      !Number.isSafeInteger(tensorByteLength) ||
      relativeStart < 0 ||
      relativeEnd - relativeStart !== tensorByteLength ||
      dataStart + relativeEnd > stats.size
    ) {
      throw new Error(`Safetensors tensor '${tensorName}' has invalid offsets`);
    }

    const sharedBuffer = new SharedArrayBuffer(tensorByteLength);
    await readExactly(
      file,
      Buffer.from(sharedBuffer),
      dataStart + relativeStart,
    );
    return {
      data:
        tensor.dtype === "F16"
          ? new Uint16Array(sharedBuffer)
          : new Float32Array(sharedBuffer),
      dimension: expectedDimension,
      dtype: tensor.dtype,
      rows,
    };
  } finally {
    await file.close();
  }
}

async function readExactly(
  file: FileHandle,
  buffer: Buffer,
  position: number,
): Promise<void> {
  let offset = 0;
  while (offset < buffer.byteLength) {
    const { bytesRead } = await file.read(
      buffer,
      offset,
      buffer.byteLength - offset,
      position + offset,
    );
    if (bytesRead === 0) {
      throw new Error("Safetensors file ended unexpectedly");
    }
    offset += bytesRead;
  }
}
