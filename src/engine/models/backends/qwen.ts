import { globalConfigPath } from "../../config.js";
import { EngineError } from "../../errors.js";
import type { Content, ImageFormat } from "../../types.js";
import {
  BaseEmbeddingModel,
  type CreateEmbeddingModelOptions,
  type EmbeddingModelInfo,
  type EmbeddingResult,
  type NormalizedEmbeddingOptions,
} from "../embeddings.js";
import type {
  QwenMultimodalEmbeddingCatalogEntry,
  QwenTextEmbeddingCatalogEntry,
} from "../catalog.js";
import { traceHeaders } from "../../../observability/trace-context.js";
import {
  OpenAiCompatibleTextEmbeddingModel,
  providerErrorContext,
  readProviderError,
  remoteEmbeddingSignal,
  throwIfEmbeddingCancelled,
  type OpenAiCompatibleDependencies,
  type OpenAiCompatibleTextEmbeddingSpec,
} from "./openai-compatible.js";

// -----------------------------------------------------------------------------
// Text embedding models (OpenAI-compatible API)
// -----------------------------------------------------------------------------

const DEFAULT_REMOTE_EMBEDDING_TIMEOUT_MS = 60_000;

type QwenDependencies = {
  fetch: typeof globalThis.fetch;
};

const defaultDependencies: QwenDependencies = {
  fetch: (...args) => globalThis.fetch(...args),
};

const QWEN_TEXT_EMBEDDING_V4_SPEC = {
  displayName: "Qwen text-embedding-v4",
  errorCodePrefix: "QWEN_TEXT_EMBEDDING_V4",
  requireApiKey: true,
  missingApiKeyHint: `Pass --api-key, set ZVEC_GREP_API_KEY, or configure providers.qwen.apiKey in ${globalConfigPath()}.`,
} as const satisfies OpenAiCompatibleTextEmbeddingSpec;

const QWEN37_TEXT_EMBEDDING_SPEC = {
  displayName: "Qwen3.7 text embedding",
  errorCodePrefix: "QWEN37_TEXT_EMBEDDING",
  requireApiKey: true,
  missingApiKeyHint: `Pass --api-key, set ZVEC_GREP_API_KEY, or configure providers.qwen.apiKey in ${globalConfigPath()}.`,
} as const satisfies OpenAiCompatibleTextEmbeddingSpec;

type QwenTextEmbeddingV4CatalogEntry = Extract<
  QwenTextEmbeddingCatalogEntry,
  { model: "text-embedding-v4" }
>;

type Qwen37TextEmbeddingCatalogEntry = Extract<
  QwenTextEmbeddingCatalogEntry,
  { model: "qwen3.7-text-embedding" }
>;

export class QwenTextEmbeddingV4Model extends OpenAiCompatibleTextEmbeddingModel {
  constructor(
    entry: QwenTextEmbeddingV4CatalogEntry,
    options: CreateEmbeddingModelOptions,
    dependencies: Partial<OpenAiCompatibleDependencies> = {},
  ) {
    super(entry, options, QWEN_TEXT_EMBEDDING_V4_SPEC, dependencies);
  }
}

export class Qwen37TextEmbeddingModel extends OpenAiCompatibleTextEmbeddingModel {
  constructor(
    entry: Qwen37TextEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
    dependencies: Partial<OpenAiCompatibleDependencies> = {},
  ) {
    super(entry, options, QWEN37_TEXT_EMBEDDING_SPEC, dependencies);
  }
}

// -----------------------------------------------------------------------------
// qwen3-vl-embedding
// -----------------------------------------------------------------------------

const QWEN3_VL_EMBEDDING_MAX_IMAGE_COUNT = 10;

const QWEN3_VL_EMBEDDING_SUPPORTED_IMAGE_FORMATS: readonly ImageFormat[] = [
  "jpeg",
  "png",
  "webp",
];

export class Qwen3VlEmbeddingModel extends BaseEmbeddingModel {
  readonly info: EmbeddingModelInfo;

  private readonly entry: QwenMultimodalEmbeddingCatalogEntry;
  private readonly apiKey: string;
  private readonly endpoint: string;
  private readonly dependencies: QwenDependencies;

  constructor(
    entry: QwenMultimodalEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
    dependencies: Partial<QwenDependencies> = {},
  ) {
    super();

    this.entry = entry;
    this.dependencies = { ...defaultDependencies, ...dependencies };
    const endpoint =
      options.endpoint === undefined
        ? entry.defaultEndpoint
        : options.endpoint.trim();
    this.info = {
      reference: entry.reference,
      provider: entry.provider,
      name: entry.model,
      dimension: entry.dimension,
      metric: entry.metric,
      endpoint,
      inputKinds: ["text", "image"],
      limits: {
        maxBatchSize: entry.maxBatchSize,
        maxInputTokens: entry.maxInputTokens,
        maxImageBytes: entry.maxImageBytes,
      },
    };

    const apiKey = options.apiKey?.trim() ?? "";
    if (apiKey.length === 0) {
      throw new EngineError("Qwen3 VL embedding model requires an API key", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_MISSING_API_KEY",
        context: `model=${this.info.reference}\nhint=Pass --api-key, set ZVEC_GREP_API_KEY, or configure providers.qwen.apiKey in ${globalConfigPath()}.`,
      });
    }

    if (endpoint.length === 0) {
      throw new EngineError("Qwen3 VL embedding model requires an endpoint", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_MISSING_ENDPOINT",
        context: `model=${this.info.reference}`,
      });
    }

    this.apiKey = apiKey;
    this.endpoint = endpoint;
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult> {
    validateQwen3VlContents(this.entry.model, contents);

    const requestContents = contents.map((content) => {
      if (content.kind === "text") {
        return {
          text: content.text,
        };
      }

      return {
        image: bytesToBase64(content.data),
      };
    });

    let response: Response;
    const signal = remoteEmbeddingSignal(options.signal);

    try {
      response = await this.dependencies.fetch(this.endpoint, {
        method: "POST",
        headers: {
          ...traceHeaders(),
          Authorization: `Bearer ${this.apiKey}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          model: this.entry.model,
          input: {
            contents: requestContents,
          },
          parameters: {
            dimension: this.info.dimension,
          },
        }),
        signal,
      });
    } catch (cause) {
      throwIfEmbeddingCancelled(options.signal);
      throw new EngineError("Qwen3 VL embedding request failed", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_REQUEST_FAILED",
        context: `model=${this.info.reference} endpoint=${this.endpoint} timeoutMs=${DEFAULT_REMOTE_EMBEDDING_TIMEOUT_MS}`,
        cause,
      });
    }

    let body: unknown;

    try {
      body = await response.json();
    } catch (cause) {
      throw new EngineError("Qwen3 VL embedding response was not valid JSON", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INVALID_JSON",
        context: `model=${this.info.reference} status=${response.status}`,
        cause,
      });
    }

    if (!response.ok) {
      const error = readProviderError(body);

      throw new EngineError("Qwen3 VL embedding request returned an error", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_API_ERROR",
        context: providerErrorContext(this.entry.model, response, error),
      });
    }

    if (
      !isRecord(body) ||
      !isRecord(body.output) ||
      !Array.isArray(body.output.embeddings)
    ) {
      throw new EngineError(
        "Qwen3 VL embedding response did not include embeddings",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_MISSING_EMBEDDINGS",
          context: `model=${this.info.reference}`,
        },
      );
    }

    const vectors = new Array<number[]>(contents.length);

    for (const [fallbackIndex, item] of body.output.embeddings.entries()) {
      if (!isRecord(item)) {
        throw new EngineError(
          "Qwen3 VL embedding response included an invalid embedding item",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INVALID_ITEM",
            context: `model=${this.info.reference} index=${fallbackIndex}`,
          },
        );
      }

      const index = readEmbeddingIndex(item, fallbackIndex);

      if (index < 0 || index >= contents.length) {
        throw new EngineError(
          "Qwen3 VL embedding response index was out of range",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INDEX_OUT_OF_RANGE",
            context: `model=${this.info.reference} index=${index} inputCount=${contents.length}`,
          },
        );
      }

      if (!Array.isArray(item.embedding)) {
        throw new EngineError(
          "Qwen3 VL embedding response included an invalid embedding",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INVALID_VECTOR",
            context: `model=${this.info.reference} index=${index}`,
          },
        );
      }

      vectors[index] = item.embedding as number[];
    }

    return { vectors, truncated: [] };
  }
}

// -----------------------------------------------------------------------------
// Shared helpers
// -----------------------------------------------------------------------------

const BASE64_CHARS =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function readEmbeddingIndex(
  item: Record<string, unknown>,
  fallbackIndex: number,
): number {
  if (typeof item.index === "number" && Number.isInteger(item.index)) {
    return item.index;
  }

  if (
    typeof item.text_index === "number" &&
    Number.isInteger(item.text_index)
  ) {
    return item.text_index;
  }

  return fallbackIndex;
}

function validateQwen3VlContents(
  model: string,
  contents: readonly Content[],
): void {
  let imageCount = 0;

  for (const [index, content] of contents.entries()) {
    if (content.kind !== "image") {
      continue;
    }

    imageCount += 1;

    if (!QWEN3_VL_EMBEDDING_SUPPORTED_IMAGE_FORMATS.includes(content.format)) {
      throw new EngineError(
        "Qwen3 VL embedding model does not support image format",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_UNSUPPORTED_IMAGE_FORMAT",
          context: `model=${model} index=${index} format=${content.format}`,
        },
      );
    }
  }

  if (imageCount > QWEN3_VL_EMBEDDING_MAX_IMAGE_COUNT) {
    throw new EngineError(
      "Qwen3 VL embedding image count exceeds model limit",
      {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_TOO_MANY_IMAGES",
        context: `model=${model} imageCount=${imageCount} maxImageCount=${QWEN3_VL_EMBEDDING_MAX_IMAGE_COUNT}`,
      },
    );
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function bytesToBase64(bytes: Uint8Array): string {
  let output = "";

  for (let index = 0; index < bytes.length; index += 3) {
    const first = bytes[index] ?? 0;
    const second = bytes[index + 1];
    const third = bytes[index + 2];

    output += BASE64_CHARS.charAt(first >> 2);
    output += BASE64_CHARS.charAt(((first & 3) << 4) | ((second ?? 0) >> 4));
    output +=
      second === undefined
        ? "="
        : BASE64_CHARS.charAt(((second & 15) << 2) | ((third ?? 0) >> 6));
    output += third === undefined ? "=" : BASE64_CHARS.charAt(third & 63);
  }

  return output;
}
