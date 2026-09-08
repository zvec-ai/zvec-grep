import { EngineError, type EngineErrorCode } from "../../errors.js";
import type { Content, TextContent } from "../../types.js";
import { traceHeaders } from "../../../observability/trace-context.js";
import {
  BaseEmbeddingModel,
  type CreateEmbeddingModelOptions,
  type EmbeddingModelInfo,
  type EmbeddingResult,
  type NormalizedEmbeddingOptions,
} from "../embeddings.js";

const DEFAULT_REMOTE_EMBEDDING_TIMEOUT_MS = 60_000;

export type OpenAiCompatibleTextEmbeddingCatalogEntry = Readonly<{
  reference: string;
  provider: string;
  model: string;
  dimension: number;
  metric: "cosine" | "dot" | "euclidean";
  defaultEndpoint?: string;
  maxBatchSize: number;
  maxInputTokens?: number;
  requestDimensions?: boolean;
  requestEncodingFormat?: boolean;
}>;

export type OpenAiCompatibleTextEmbeddingSpec = Readonly<{
  displayName: string;
  errorCodePrefix: string;
  requireApiKey?: boolean;
  missingApiKeyHint?: string;
}>;

export type OpenAiCompatibleDependencies = {
  fetch: typeof globalThis.fetch;
};

const defaultDependencies: OpenAiCompatibleDependencies = {
  fetch: (...args) => globalThis.fetch(...args),
};

export class OpenAiCompatibleTextEmbeddingModel extends BaseEmbeddingModel {
  readonly info: EmbeddingModelInfo;

  private readonly entry: OpenAiCompatibleTextEmbeddingCatalogEntry;
  private readonly apiKey: string;
  private readonly endpoint: string;
  private readonly displayName: string;
  private readonly errorCodePrefix: string;
  private readonly dependencies: OpenAiCompatibleDependencies;

  constructor(
    entry: OpenAiCompatibleTextEmbeddingCatalogEntry,
    options: CreateEmbeddingModelOptions,
    spec: OpenAiCompatibleTextEmbeddingSpec = {
      displayName: "OpenAI-compatible text embedding",
      errorCodePrefix: "OPENAI_COMPATIBLE_TEXT_EMBEDDING",
    },
    dependencies: Partial<OpenAiCompatibleDependencies> = {},
  ) {
    super();

    this.entry = entry;
    const endpoint =
      options.endpoint === undefined
        ? (entry.defaultEndpoint?.trim() ?? "")
        : options.endpoint.trim();
    this.info = {
      reference: entry.reference,
      provider: entry.provider,
      name: entry.model,
      dimension: entry.dimension,
      metric: entry.metric,
      endpoint,
      inputKinds: ["text"],
      limits: {
        maxBatchSize: entry.maxBatchSize,
        maxInputTokens: entry.maxInputTokens,
      },
    };
    this.displayName = spec.displayName;
    this.errorCodePrefix = spec.errorCodePrefix;
    this.dependencies = { ...defaultDependencies, ...dependencies };

    this.apiKey = options.apiKey?.trim() ?? "";
    if (spec.requireApiKey && this.apiKey.length === 0) {
      throw new EngineError(`${this.displayName} model requires an API key`, {
        code: this.errorCode("MISSING_API_KEY"),
        context: [
          `model=${this.info.reference}`,
          ...(spec.missingApiKeyHint ? [`hint=${spec.missingApiKeyHint}`] : []),
        ].join("\n"),
      });
    }

    if (endpoint.length === 0) {
      throw new EngineError(`${this.displayName} model requires an endpoint`, {
        code: this.errorCode("MISSING_ENDPOINT"),
        context: `model=${this.info.reference}`,
      });
    }

    this.endpoint = endpoint;
  }

  protected async doEmbed(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult> {
    const texts = (contents as readonly TextContent[]).map(
      (content) => content.text,
    );
    const requestBody: Record<string, unknown> = {
      model: this.entry.model,
      input: texts,
    };
    if (this.entry.requestDimensions) {
      requestBody.dimensions = this.info.dimension;
    }
    if (this.entry.requestEncodingFormat) {
      requestBody.encoding_format = "float";
    }

    const headers: Record<string, string> = {
      ...traceHeaders(),
      "Content-Type": "application/json",
    };
    if (this.apiKey.length > 0) {
      headers.Authorization = `Bearer ${this.apiKey}`;
    }

    let response: Response;
    const signal = remoteEmbeddingSignal(options.signal);

    try {
      response = await this.dependencies.fetch(this.endpoint, {
        method: "POST",
        headers,
        body: JSON.stringify(requestBody),
        signal,
      });
    } catch (cause) {
      throwIfEmbeddingCancelled(options.signal);
      throw new EngineError(`${this.displayName} request failed`, {
        code: this.errorCode("REQUEST_FAILED"),
        context: `model=${this.info.reference} endpoint=${this.endpoint} timeoutMs=${DEFAULT_REMOTE_EMBEDDING_TIMEOUT_MS}`,
        cause,
      });
    }

    let body: unknown;
    try {
      body = await response.json();
    } catch (cause) {
      throw new EngineError(`${this.displayName} response was not valid JSON`, {
        code: this.errorCode("INVALID_JSON"),
        context: `model=${this.info.reference} status=${response.status}`,
        cause,
      });
    }

    if (!response.ok) {
      const error = readProviderError(body);
      throw new EngineError(`${this.displayName} request returned an error`, {
        code: this.errorCode("API_ERROR"),
        context: providerErrorContext(this.entry.model, response, error),
      });
    }

    if (!isRecord(body) || !Array.isArray(body.data)) {
      throw new EngineError(
        `${this.displayName} response did not include data`,
        {
          code: this.errorCode("MISSING_DATA"),
          context: `model=${this.info.reference}`,
        },
      );
    }

    const vectors = new Array<number[]>(texts.length);
    const seenIndexes = new Set<number>();

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
            context: `model=${this.info.reference} index=${isRecord(item) ? String(item.index) : "unknown"}`,
          },
        );
      }

      if (item.index < 0 || item.index >= texts.length) {
        throw new EngineError(
          `${this.displayName} response index was out of range`,
          {
            code: this.errorCode("INDEX_OUT_OF_RANGE"),
            context: `model=${this.info.reference} index=${item.index} inputCount=${texts.length}`,
          },
        );
      }

      if (seenIndexes.has(item.index)) {
        throw new EngineError(
          `${this.displayName} response included a duplicate index`,
          {
            code: this.errorCode("DUPLICATE_INDEX"),
            context: `model=${this.info.reference} index=${item.index}`,
          },
        );
      }
      seenIndexes.add(item.index);

      if (!Array.isArray(item.embedding)) {
        throw new EngineError(
          `${this.displayName} response included an invalid embedding`,
          {
            code: this.errorCode("INVALID_VECTOR"),
            context: `model=${this.info.reference} index=${item.index}`,
          },
        );
      }

      vectors[item.index] = item.embedding as number[];
    }

    return { vectors, truncated: [] };
  }

  private errorCode(suffix: string): EngineErrorCode {
    return `ZVEC_GREP.ENGINE.MODELS.${this.errorCodePrefix}_${suffix}`;
  }
}

export function providerErrorContext(
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
  if (!value) return undefined;

  const seconds = Number(value);
  if (Number.isFinite(seconds) && seconds >= 0) {
    return Math.round(seconds * 1000);
  }

  const dateMs = Date.parse(value);
  return Number.isFinite(dateMs) ? Math.max(0, dateMs - Date.now()) : undefined;
}

export function readProviderError(body: unknown): {
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
    return { code: "unknown", type: "unknown", message: "unknown" };
  }

  return {
    code: typeof body.error.code === "string" ? body.error.code : "unknown",
    type: typeof body.error.type === "string" ? body.error.type : "unknown",
    message:
      typeof body.error.message === "string" ? body.error.message : "unknown",
  };
}

export function remoteEmbeddingSignal(
  signal: AbortSignal | undefined,
): AbortSignal {
  const timeout = AbortSignal.timeout(DEFAULT_REMOTE_EMBEDDING_TIMEOUT_MS);
  return signal ? AbortSignal.any([signal, timeout]) : timeout;
}

export function throwIfEmbeddingCancelled(
  signal: AbortSignal | undefined,
): void {
  if (!signal?.aborted) return;
  throw signal.reason instanceof Error
    ? signal.reason
    : new Error("Embedding request was cancelled.");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
