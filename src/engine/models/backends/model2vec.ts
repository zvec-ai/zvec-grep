import { createWriteStream, existsSync, statSync } from "node:fs";
import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import type { ReadableStream as NodeReadableStream } from "node:stream/web";
import { AutoTokenizer } from "@huggingface/transformers";
import { EngineError } from "../../errors/index.js";
import type { Content, TextContent } from "../../types.js";
import { defaultHome } from "../../utils/path.js";
import {
  BaseEmbeddingModel,
  type NormalizedEmbeddingOptions,
} from "../embeddings.js";
import type {
  CreateEmbeddingModelOptions,
  EmbeddingModelInfo,
  EmbeddingResult,
} from "../embeddings.js";
import type { Model2VecEmbeddingCatalogEntry } from "../catalog.js";

type TokenTensor = {
  data: ArrayLike<number | bigint>;
};

type TokenizerOutput = {
  input_ids: TokenTensor;
};

type TokenizerLike = {
  (
    text: string,
    options: {
      add_special_tokens: false;
      truncation: true;
      max_length: number;
    },
  ): TokenizerOutput | Promise<TokenizerOutput>;
  unk_token_id?: number | null;
};

type StaticEmbeddingTable = {
  data: Float32Array | Uint16Array;
  dimension: number;
  dtype: "F16" | "F32";
  rows: number;
};

type Model2VecRuntime = {
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
  download(url: string, destination: string): Promise<void>;
};

const DEFAULT_MODEL_CACHE_DIR = join(defaultHome(), "models");
let runtimeOverride: Partial<Model2VecRuntime> | null = null;

const defaultRuntime: Model2VecRuntime = {
  async loadTokenizer(repo, options) {
    return (await AutoTokenizer.from_pretrained(
      repo,
      options,
    )) as unknown as TokenizerLike;
  },
  async loadSafetensors(path, tensorName, dimension) {
    return await readStaticEmbeddingTable(path, tensorName, dimension);
  },
  async download(url, destination) {
    const response = await fetch(url, { redirect: "follow" });
    if (!response.ok || !response.body) {
      throw new Error(`HTTP ${response.status} ${response.statusText}`);
    }
    await pipeline(
      Readable.fromWeb(response.body as NodeReadableStream),
      createWriteStream(destination),
    );
  },
};

export function setModel2VecRuntimeForTesting(
  runtime: Partial<Model2VecRuntime> | null,
): void {
  runtimeOverride = runtime;
}

export class Model2VecEmbeddingModel extends BaseEmbeddingModel {
  readonly info: EmbeddingModelInfo;

  private readonly modelCacheDir: string;
  private tokenizer: TokenizerLike | null = null;
  private staticTable: StaticEmbeddingTable | null = null;
  private loadPromise: Promise<void> | null = null;
  private disposed = false;

  constructor(
    private readonly entry: Model2VecEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
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
        maxInputTokens: entry.maxInputTokens,
      },
    };
    this.modelCacheDir =
      options.modelCacheDir ??
      process.env.ZVEC_GREP_MODEL_CACHE ??
      DEFAULT_MODEL_CACHE_DIR;
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
    await this.ensureLoaded();

    try {
      const texts = (contents as readonly TextContent[]).map((content) =>
        formatText(content.text, options.purpose, this.entry),
      );
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
    this.staticTable = null;
    this.tokenizer = null;
    this.loadPromise = null;
  }

  private async ensureLoaded(): Promise<void> {
    if (this.tokenizer && this.staticTable) {
      return;
    }
    if (this.loadPromise) {
      return await this.loadPromise;
    }

    this.loadPromise = this.loadModel();
    try {
      await this.loadPromise;
    } finally {
      this.loadPromise = null;
    }
  }

  private async loadModel(): Promise<void> {
    const runtime: Model2VecRuntime = runtimeOverride
      ? { ...defaultRuntime, ...runtimeOverride }
      : defaultRuntime;
    const modelPath = await this.resolveModelPath(runtime);
    const tokenizerSource = await this.resolveTokenizerSource(runtime);
    const tokenizerPromise = runtime.loadTokenizer(tokenizerSource, {
      cache_dir: this.modelCacheDir,
      revision: this.entry.revision,
      ...(tokenizerSource !== this.entry.repo
        ? { local_files_only: true }
        : {}),
    });
    const [tokenizer, staticTable] = await Promise.all([
      tokenizerPromise,
      runtime.loadSafetensors(
        modelPath,
        this.entry.embeddingTensor,
        this.info.dimension,
      ),
    ]);
    this.ensureNotDisposed();
    this.tokenizer = tokenizer;
    this.staticTable = staticTable;
  }

  private async resolveModelPath(runtime: Model2VecRuntime): Promise<string> {
    const modelPath = join(
      this.modelCacheDir,
      "model2vec",
      this.entry.repo.replaceAll("/", "--"),
      this.entry.revision,
      basename(this.entry.modelFile),
    );
    return await this.resolveCachedFile(
      runtime,
      this.entry.modelFile,
      modelPath,
    );
  }

  private async resolveTokenizerSource(
    runtime: Model2VecRuntime,
  ): Promise<string> {
    const tokenizerDirectory = join(
      this.modelCacheDir,
      "model2vec",
      this.entry.repo.replaceAll("/", "--"),
      this.entry.revision,
      "tokenizer",
    );
    await this.resolveCachedFile(
      runtime,
      this.entry.tokenizerFile,
      join(tokenizerDirectory, "tokenizer.json"),
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
    runtime: Model2VecRuntime,
    remoteFile: string,
    localPath: string,
  ): Promise<string> {
    if (isUsableModelFile(localPath)) {
      return localPath;
    }

    await mkdir(dirname(localPath), { recursive: true });
    const partialPath = `${localPath}.part-${process.pid}-${Date.now()}`;
    const url = `https://huggingface.co/${this.entry.repo}/resolve/${this.entry.revision}/${remoteFile}`;
    try {
      await runtime.download(url, partialPath);
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

    const truncatedInputIndexes: number[] = [];
    const tokenLists = await Promise.all(
      texts.map(async (text, index) => {
        const encoded = await tokenizer(text, {
          add_special_tokens: false,
          truncation: true,
          max_length: this.entry.maxInputTokens + 1,
        });
        const encodedTokenIds = Array.from(encoded.input_ids.data, Number);
        if (encodedTokenIds.length > this.entry.maxInputTokens) {
          truncatedInputIndexes.push(index);
        }
        const unknownTokenId = tokenizer.unk_token_id;
        return encodedTokenIds
          .slice(0, this.entry.maxInputTokens)
          .filter(
            (id) =>
              unknownTokenId === undefined ||
              unknownTokenId === null ||
              id !== unknownTokenId,
          );
      }),
    );

    return {
      vectors: embedStaticTokenLists(
        tokenLists,
        staticTable,
        this.info.dimension,
        this.entry.normalize,
      ),
      truncated: truncatedInputIndexes.sort((left, right) => left - right),
    };
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
  const file = await readFile(path);
  if (file.byteLength < 9) {
    throw new Error("Safetensors file is too small");
  }

  const headerLength = Number(file.readBigUInt64LE(0));
  const dataStart = 8 + headerLength;
  if (!Number.isSafeInteger(headerLength) || dataStart > file.byteLength) {
    throw new Error("Safetensors header length is invalid");
  }

  let header: Record<
    string,
    { data_offsets?: [number, number]; dtype?: string; shape?: number[] }
  >;
  try {
    header = JSON.parse(
      file.subarray(8, dataStart).toString("utf8"),
    ) as typeof header;
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
  if (
    relativeStart < 0 ||
    relativeEnd - relativeStart !== valueCount * bytesPerValue ||
    dataStart + relativeEnd > file.byteLength
  ) {
    throw new Error(`Safetensors tensor '${tensorName}' has invalid offsets`);
  }

  const byteOffset = file.byteOffset + dataStart + relativeStart;
  const data =
    tensor.dtype === "F16"
      ? new Uint16Array(file.buffer, byteOffset, valueCount)
      : new Float32Array(file.buffer, byteOffset, valueCount);
  return {
    data,
    dimension: expectedDimension,
    dtype: tensor.dtype,
    rows,
  };
}

function embedStaticTokenLists(
  tokenLists: readonly number[][],
  table: StaticEmbeddingTable,
  dimension: number,
  normalize: boolean,
): number[][] {
  const halfValues = table.dtype === "F16" ? halfFloatValues() : null;
  return tokenLists.map((tokenIds) => {
    const vector = Array.from({ length: dimension }, () => 0);
    if (tokenIds.length === 0) {
      return vector;
    }

    for (const tokenId of tokenIds) {
      if (!Number.isInteger(tokenId) || tokenId < 0 || tokenId >= table.rows) {
        throw new Error(
          `Tokenizer returned out-of-range token id: id=${tokenId} rows=${table.rows}`,
        );
      }
      const start = tokenId * dimension;
      for (let column = 0; column < dimension; column++) {
        const value = table.data[start + column];
        vector[column] += halfValues ? halfValues[value] : value;
      }
    }

    let squaredNorm = 0;
    for (let column = 0; column < dimension; column++) {
      vector[column] /= tokenIds.length;
      squaredNorm += vector[column] * vector[column];
    }
    if (normalize && squaredNorm > 0) {
      const inverseNorm = 1 / Math.sqrt(squaredNorm);
      for (let column = 0; column < dimension; column++) {
        vector[column] *= inverseNorm;
      }
    }
    return vector;
  });
}

let cachedHalfFloatValues: Float32Array | null = null;

function halfFloatValues(): Float32Array {
  if (cachedHalfFloatValues) {
    return cachedHalfFloatValues;
  }
  const values = new Float32Array(65_536);
  for (let bits = 0; bits < values.length; bits++) {
    const sign = bits & 0x8000 ? -1 : 1;
    const exponent = (bits >> 10) & 0x1f;
    const fraction = bits & 0x03ff;
    if (exponent === 0) {
      values[bits] = sign * 2 ** -14 * (fraction / 1024);
    } else if (exponent === 0x1f) {
      values[bits] = fraction === 0 ? sign * Infinity : Number.NaN;
    } else {
      values[bits] = sign * 2 ** (exponent - 15) * (1 + fraction / 1024);
    }
  }
  cachedHalfFloatValues = values;
  return values;
}
