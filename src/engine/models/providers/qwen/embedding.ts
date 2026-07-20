import { EngineError, type EngineErrorCode } from "../../../errors/index.js";
import { globalConfigPath } from "../../../config.js";
import type { Content, ImageFormat, TextContent } from "../../../types.js";
import {
  EmbeddingModel,
  type EmbeddingLimits,
  type EmbeddingOptions,
  type EmbeddingVector,
} from "../../embeddings.js";
import type { ModelProviderOptions, ModelRef } from "../../types.js";

// -----------------------------------------------------------------------------
// Text embedding models (OpenAI-compatible API)
// -----------------------------------------------------------------------------

const DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT =
  "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings";

type QwenTextEmbeddingSpec = {
  model: string;
  displayName: string;
  errorCodePrefix: string;
  maxBatchSize: number;
  maxInputTokens: number;
};

const QWEN_TEXT_EMBEDDING_V4_SPEC = {
  model: "text-embedding-v4",
  displayName: "Qwen text-embedding-v4",
  errorCodePrefix: "QWEN_TEXT_EMBEDDING_V4",
  maxBatchSize: 10,
  maxInputTokens: 8192,
} as const satisfies QwenTextEmbeddingSpec;

const QWEN37_TEXT_EMBEDDING_SPEC = {
  model: "qwen3.7-text-embedding",
  displayName: "Qwen3.7 text embedding",
  errorCodePrefix: "QWEN37_TEXT_EMBEDDING",
  maxBatchSize: 20,
  maxInputTokens: 128000,
} as const satisfies QwenTextEmbeddingSpec;

abstract class QwenTextEmbeddingModel extends EmbeddingModel {
  readonly ref: { readonly provider: "qwen"; readonly model: string };
  readonly dimension = 1024;
  readonly metric = "cosine";
  readonly supportedContentKinds = ["text"] as const;
  readonly limits: EmbeddingLimits;
  override readonly recommendedIndexConcurrency = 8;
  override readonly maxIndexConcurrency = 12;

  private readonly apiKey: string;
  private readonly endpoint: string;
  private readonly displayName: string;
  private readonly errorCodePrefix: string;
  private readonly authorizeRemoteEmbedding?: ModelProviderOptions["authorizeRemoteEmbedding"];

  constructor(spec: QwenTextEmbeddingSpec, options: ModelProviderOptions) {
    super();

    this.ref = {
      provider: "qwen",
      model: spec.model,
    };
    this.limits = {
      maxBatchSize: spec.maxBatchSize,
      maxInputTokens: spec.maxInputTokens,
    };
    this.displayName = spec.displayName;
    this.errorCodePrefix = spec.errorCodePrefix;

    if (options.apiKey.trim().length === 0) {
      throw new EngineError(`${this.displayName} model requires an API key`, {
        code: this.errorCode("MISSING_API_KEY"),
        context: `model=${this.ref.model}\nhint=Pass --api-key, set ZVEC_GREP_API_KEY, or configure providers.qwen.apiKey in ${globalConfigPath()}.`,
      });
    }

    const endpoint = resolveRemoteEmbeddingEndpoint(this.ref, options.endpoint);

    if (endpoint.length === 0) {
      throw new EngineError(`${this.displayName} model requires an endpoint`, {
        code: this.errorCode("MISSING_ENDPOINT"),
        context: `model=${this.ref.model}`,
      });
    }

    this.apiKey = options.apiKey;
    this.endpoint = endpoint;
    this.authorizeRemoteEmbedding = options.authorizeRemoteEmbedding;
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: Required<EmbeddingOptions>,
  ): Promise<EmbeddingVector[]> {
    const texts = (contents as readonly TextContent[]).map(
      (content) => content.text,
    );

    await this.authorizeRemoteEmbedding?.({
      provider: this.ref.provider,
      model: this.ref.model,
      endpoint: this.endpoint,
      purpose: options.purpose,
      contentKinds: contents.map((content) => content.kind),
      contentCount: contents.length,
    });

    let response: Response;

    try {
      response = await fetch(this.endpoint, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.apiKey}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          model: this.ref.model,
          input: texts,
          dimensions: this.dimension,
          encoding_format: "float",
        }),
      });
    } catch (cause) {
      throw new EngineError(`${this.displayName} request failed`, {
        code: this.errorCode("REQUEST_FAILED"),
        context: `model=${this.ref.model} endpoint=${this.endpoint}`,
        cause,
      });
    }

    let body: unknown;

    try {
      body = await response.json();
    } catch (cause) {
      throw new EngineError(`${this.displayName} response was not valid JSON`, {
        code: this.errorCode("INVALID_JSON"),
        context: `model=${this.ref.model} status=${response.status}`,
        cause,
      });
    }

    if (!response.ok) {
      const error = readProviderError(body);

      throw new EngineError(`${this.displayName} request returned an error`, {
        code: this.errorCode("API_ERROR"),
        context: providerErrorContext(this.ref.model, response, error),
      });
    }

    if (!isRecord(body) || !Array.isArray(body.data)) {
      throw new EngineError(
        `${this.displayName} response did not include data`,
        {
          code: this.errorCode("MISSING_DATA"),
          context: `model=${this.ref.model}`,
        },
      );
    }

    const vectors = new Array<EmbeddingVector>(texts.length);

    for (const item of body.data) {
      if (
        !isRecord(item) ||
        typeof item.index !== "number" ||
        !Number.isInteger(item.index)
      ) {
        throw new EngineError(
          `${this.displayName} response included an invalid index`,
          {
            code: this.errorCode("INVALID_INDEX"),
            context: `model=${this.ref.model} index=${isRecord(item) ? String(item.index) : "unknown"}`,
          },
        );
      }

      if (item.index < 0 || item.index >= texts.length) {
        throw new EngineError(
          `${this.displayName} response index was out of range`,
          {
            code: this.errorCode("INDEX_OUT_OF_RANGE"),
            context: `model=${this.ref.model} index=${item.index} inputCount=${texts.length}`,
          },
        );
      }

      if (!Array.isArray(item.embedding)) {
        throw new EngineError(
          `${this.displayName} response included an invalid embedding`,
          {
            code: this.errorCode("INVALID_VECTOR"),
            context: `model=${this.ref.model} index=${item.index}`,
          },
        );
      }

      vectors[item.index] = item.embedding as EmbeddingVector;
    }

    return vectors;
  }

  private errorCode(suffix: string): EngineErrorCode {
    return `ZVEC_GREP.ENGINE.MODELS.${this.errorCodePrefix}_${suffix}`;
  }
}

export class QwenTextEmbeddingV4Model extends QwenTextEmbeddingModel {
  constructor(options: ModelProviderOptions) {
    super(QWEN_TEXT_EMBEDDING_V4_SPEC, options);
  }
}

export class Qwen37TextEmbeddingModel extends QwenTextEmbeddingModel {
  constructor(options: ModelProviderOptions) {
    super(QWEN37_TEXT_EMBEDDING_SPEC, options);
  }
}

// -----------------------------------------------------------------------------
// qwen3-vl-embedding
// -----------------------------------------------------------------------------

const DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT =
  "https://dashscope.aliyuncs.com/api/v1/services/embeddings/multimodal-embedding/multimodal-embedding";

const QWEN3_VL_EMBEDDING_MAX_IMAGE_COUNT = 10;

const QWEN3_VL_EMBEDDING_SUPPORTED_IMAGE_FORMATS: readonly ImageFormat[] = [
  "jpeg",
  "png",
  "webp",
];

export class Qwen3VlEmbeddingModel extends EmbeddingModel {
  readonly ref = {
    provider: "qwen",
    model: "qwen3-vl-embedding",
  } as const;
  readonly dimension = 2560;
  readonly metric = "cosine";
  readonly supportedContentKinds = ["text", "image"] as const;
  readonly limits = {
    maxBatchSize: 20,
    maxInputTokens: 32000,
    maxImageBytes: 10 * 1024 * 1024,
  } as const satisfies EmbeddingLimits;
  override readonly recommendedIndexConcurrency = 4;
  override readonly maxIndexConcurrency = 8;

  private readonly apiKey: string;
  private readonly endpoint: string;
  private readonly authorizeRemoteEmbedding?: ModelProviderOptions["authorizeRemoteEmbedding"];

  constructor(options: ModelProviderOptions) {
    super();

    if (options.apiKey.trim().length === 0) {
      throw new EngineError("Qwen3 VL embedding model requires an API key", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_MISSING_API_KEY",
        context: `model=${this.ref.model}\nhint=Pass --api-key, set ZVEC_GREP_API_KEY, or configure providers.qwen.apiKey in ${globalConfigPath()}.`,
      });
    }

    const endpoint = resolveRemoteEmbeddingEndpoint(this.ref, options.endpoint);

    if (endpoint.length === 0) {
      throw new EngineError("Qwen3 VL embedding model requires an endpoint", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_MISSING_ENDPOINT",
        context: `model=${this.ref.model}`,
      });
    }

    this.apiKey = options.apiKey;
    this.endpoint = endpoint;
    this.authorizeRemoteEmbedding = options.authorizeRemoteEmbedding;
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: Required<EmbeddingOptions>,
  ): Promise<EmbeddingVector[]> {
    validateQwen3VlContents(this.ref.model, contents);

    await this.authorizeRemoteEmbedding?.({
      provider: this.ref.provider,
      model: this.ref.model,
      endpoint: this.endpoint,
      purpose: options.purpose,
      contentKinds: contents.map((content) => content.kind),
      contentCount: contents.length,
    });

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

    try {
      response = await fetch(this.endpoint, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.apiKey}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          model: this.ref.model,
          input: {
            contents: requestContents,
          },
          parameters: {
            dimension: this.dimension,
          },
        }),
      });
    } catch (cause) {
      throw new EngineError("Qwen3 VL embedding request failed", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_REQUEST_FAILED",
        context: `model=${this.ref.model} endpoint=${this.endpoint}`,
        cause,
      });
    }

    let body: unknown;

    try {
      body = await response.json();
    } catch (cause) {
      throw new EngineError("Qwen3 VL embedding response was not valid JSON", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INVALID_JSON",
        context: `model=${this.ref.model} status=${response.status}`,
        cause,
      });
    }

    if (!response.ok) {
      const error = readProviderError(body);

      throw new EngineError("Qwen3 VL embedding request returned an error", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_API_ERROR",
        context: providerErrorContext(this.ref.model, response, error),
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
          context: `model=${this.ref.model}`,
        },
      );
    }

    const vectors = new Array<EmbeddingVector>(contents.length);

    for (const [fallbackIndex, item] of body.output.embeddings.entries()) {
      if (!isRecord(item)) {
        throw new EngineError(
          "Qwen3 VL embedding response included an invalid embedding item",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INVALID_ITEM",
            context: `model=${this.ref.model} index=${fallbackIndex}`,
          },
        );
      }

      const index = readEmbeddingIndex(item, fallbackIndex);

      if (index < 0 || index >= contents.length) {
        throw new EngineError(
          "Qwen3 VL embedding response index was out of range",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INDEX_OUT_OF_RANGE",
            context: `model=${this.ref.model} index=${index} inputCount=${contents.length}`,
          },
        );
      }

      if (!Array.isArray(item.embedding)) {
        throw new EngineError(
          "Qwen3 VL embedding response included an invalid embedding",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.QWEN3_VL_EMBEDDING_INVALID_VECTOR",
            context: `model=${this.ref.model} index=${index}`,
          },
        );
      }

      vectors[index] = item.embedding as EmbeddingVector;
    }

    return vectors;
  }
}

// -----------------------------------------------------------------------------
// Shared helpers
// -----------------------------------------------------------------------------

const BASE64_CHARS =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function normalizeEndpoint(endpoint: string): string {
  return endpoint.trim();
}

export function resolveRemoteEmbeddingEndpoint(
  ref: ModelRef,
  endpoint?: string,
): string {
  if (endpoint !== undefined) return normalizeEndpoint(endpoint);
  if (
    ref.provider === "qwen" &&
    (ref.model === "text-embedding-v4" ||
      ref.model === "qwen3.7-text-embedding")
  ) {
    return DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT;
  }
  if (ref.provider === "qwen" && ref.model === "qwen3-vl-embedding") {
    return DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT;
  }
  throw new EngineError("Remote embedding endpoint is not implemented", {
    code: "ZVEC_GREP.ENGINE.MODELS.REMOTE_ENDPOINT_NOT_IMPLEMENTED",
    context: `provider=${ref.provider} model=${ref.model}`,
  });
}

function providerErrorContext(
  model: string,
  response: Response,
  error: { code: string; type: string; message: string },
): string {
  const retryAfter = retryAfterHeaderMs(response.headers.get("retry-after"));
  const retryAfterDetail =
    typeof retryAfter === "number" ? ` retryAfterMs=${retryAfter}` : "";

  return `model=${model} status=${response.status}${retryAfterDetail} providerCode=${error.code} providerType=${error.type} providerMessage=${error.message}`;
}

function retryAfterHeaderMs(value: string | null): number | undefined {
  if (!value) {
    return undefined;
  }

  const seconds = Number(value);
  if (Number.isFinite(seconds) && seconds >= 0) {
    return Math.round(seconds * 1000);
  }

  const dateMs = Date.parse(value);
  if (Number.isFinite(dateMs)) {
    return Math.max(0, dateMs - Date.now());
  }

  return undefined;
}

function readProviderError(body: unknown): {
  code: string;
  type: string;
  message: string;
} {
  if (!isRecord(body) || !isRecord(body.error)) {
    if (isRecord(body)) {
      return {
        code: typeof body.code === "string" ? body.code : "unknown",
        type: "unknown",
        message: typeof body.message === "string" ? body.message : "unknown",
      };
    }

    return {
      code: "unknown",
      type: "unknown",
      message: "unknown",
    };
  }

  return {
    code: typeof body.error.code === "string" ? body.error.code : "unknown",
    type: typeof body.error.type === "string" ? body.error.type : "unknown",
    message:
      typeof body.error.message === "string" ? body.error.message : "unknown",
  };
}

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
