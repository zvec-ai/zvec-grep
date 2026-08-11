import { createWriteStream, existsSync, statSync } from "node:fs";
import { mkdir, open, rename, rm, writeFile } from "node:fs/promises";
import type { FileHandle } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import type { ReadableStream as NodeReadableStream } from "node:stream/web";
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
import type { Model2VecEmbeddingCatalogEntry } from "../catalog.js";
import {
  createModelDownloadProgressReporter,
  type ModelDownloadProgressReporter,
} from "../download-progress.js";
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
  download(
    url: string,
    destination: string,
    onProgress?: (progress: {
      downloadedBytes: number;
      totalBytes?: number;
    }) => void,
  ): Promise<void>;
};

const DEFAULT_MODEL_CACHE_DIR = join(defaultHome(), "models");

const defaultDependencies: Model2VecDependencies = {
  async loadTokenizer(source) {
    return await loadModel2VecTokenizer(source);
  },
  async loadSafetensors(path, tensorName, dimension) {
    return await readStaticEmbeddingTable(path, tensorName, dimension);
  },
  async download(url, destination, onProgress) {
    const response = await fetch(url, { redirect: "follow" });
    if (!response.ok || !response.body) {
      throw new Error(`HTTP ${response.status} ${response.statusText}`);
    }
    const contentLength = response.headers.get("content-length");
    const parsedTotalBytes = contentLength
      ? Number.parseInt(contentLength, 10)
      : undefined;
    const totalBytes =
      parsedTotalBytes !== undefined &&
      Number.isFinite(parsedTotalBytes) &&
      parsedTotalBytes >= 0
        ? parsedTotalBytes
        : undefined;
    let downloadedBytes = 0;
    const progressStream = new Transform({
      transform(chunk, _encoding, callback) {
        downloadedBytes += Buffer.byteLength(chunk);
        onProgress?.({ downloadedBytes, totalBytes });
        callback(null, chunk);
      },
    });
    await pipeline(
      Readable.fromWeb(response.body as NodeReadableStream),
      progressStream,
      createWriteStream(destination),
    );
  },
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
    this.modelCacheDir =
      options.modelCacheDir ??
      process.env.ZVEC_GREP_MODEL_CACHE ??
      DEFAULT_MODEL_CACHE_DIR;
    this.dependencies = { ...defaultDependencies, ...dependencies };
    this.useWorkerPool = Object.keys(dependencies).length === 0;
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
      [basename(this.entry.modelFile), basename(this.entry.tokenizerFile)],
    );
    downloadProgress.start();
    const [modelPath, tokenizerSource] = await Promise.all([
      this.resolveModelPath(downloadProgress),
      this.resolveTokenizerSource(downloadProgress),
    ]);
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
      this.tokenizer = await this.dependencies.loadTokenizer(tokenizerSource, {
        cache_dir: this.modelCacheDir,
        revision: this.entry.revision,
        ...(tokenizerSource !== this.entry.repo
          ? { local_files_only: true }
          : {}),
      });
      this.ensureNotDisposed();
      this.staticTable = staticTable;
    }
    downloadProgress.finish();
  }

  private async resolveModelPath(
    downloadProgress: ModelDownloadProgressReporter,
  ): Promise<string> {
    const modelPath = join(
      this.modelCacheDir,
      "model2vec",
      this.entry.repo.replaceAll("/", "--"),
      this.entry.revision,
      basename(this.entry.modelFile),
    );
    return await this.resolveCachedFile(
      this.entry.modelFile,
      modelPath,
      downloadProgress,
    );
  }

  private async resolveTokenizerSource(
    downloadProgress: ModelDownloadProgressReporter,
  ): Promise<string> {
    const tokenizerDirectory = join(
      this.modelCacheDir,
      "model2vec",
      this.entry.repo.replaceAll("/", "--"),
      this.entry.revision,
      "tokenizer",
    );
    await this.resolveCachedFile(
      this.entry.tokenizerFile,
      join(tokenizerDirectory, "tokenizer.json"),
      downloadProgress,
    );
    const configPath = join(tokenizerDirectory, "tokenizer_config.json");
    if (!isUsableModelFile(configPath)) {
      await writeFile(
        configPath,
        `${JSON.stringify({ tokenizer_class: "PreTrainedTokenizer" })}\n`,
      );
    }
    return tokenizerDirectory;
  }

  private async resolveCachedFile(
    remoteFile: string,
    localPath: string,
    downloadProgress: ModelDownloadProgressReporter,
  ): Promise<string> {
    if (isUsableModelFile(localPath)) {
      downloadProgress.skip(basename(remoteFile));
      return localPath;
    }

    await mkdir(dirname(localPath), { recursive: true });
    const partialPath = `${localPath}.part-${process.pid}-${Date.now()}`;
    const url = `https://huggingface.co/${this.entry.repo}/resolve/${this.entry.revision}/${remoteFile}`;
    try {
      await this.dependencies.download(url, partialPath, (progress) => {
        downloadProgress.report({
          artifact: basename(remoteFile),
          ...progress,
        });
      });
      if (!isUsableModelFile(partialPath)) {
        throw new Error("Downloaded model file is empty");
      }
      await rename(partialPath, localPath);
      return localPath;
    } catch (cause) {
      await rm(partialPath, { force: true });
      throw new EngineError("Unable to download Model2Vec model artifact", {
        code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED",
        context: `model=${this.entry.reference} url=${url}`,
        cause,
      });
    }
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

function isUsableModelFile(path: string): boolean {
  return existsSync(path) && statSync(path).size > 0;
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
