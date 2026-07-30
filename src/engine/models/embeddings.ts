import { EngineError } from "../errors/index.js";
import type { Content, ContentKind } from "../types.js";

export type CreateEmbeddingModelOptions = {
  apiKey?: string;
  endpoint?: string;
  modelCacheDir?: string;
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
};

export const EmbeddingPurpose = {
  Document: "document",
  Query: "query",
} as const;

export type EmbeddingPurpose =
  (typeof EmbeddingPurpose)[keyof typeof EmbeddingPurpose];

export type EmbeddingOptions = {
  purpose?: EmbeddingPurpose;
  signal?: AbortSignal;
};

export type EmbeddingResult = {
  vectors: number[][];
  truncated: number[];
};

export type EmbeddingModelInfo = Readonly<{
  reference: string;
  provider: string;
  name: string;
  dimension: number;
  metric: "cosine" | "dot" | "euclidean";
  endpoint?: string;
  inputKinds: readonly ContentKind[];
  limits: Readonly<{
    maxBatchSize: number;
    maxInputTokens?: number;
    maxImageBytes?: number;
  }>;
}>;

export interface EmbeddingModel {
  readonly info: EmbeddingModelInfo;

  embed(
    contents: readonly Content[],
    options?: EmbeddingOptions,
  ): Promise<EmbeddingResult>;

  dispose(): Promise<void>;
}

export type NormalizedEmbeddingOptions = {
  purpose: EmbeddingPurpose;
  signal?: AbortSignal;
};

export abstract class BaseEmbeddingModel implements EmbeddingModel {
  abstract readonly info: EmbeddingModelInfo;

  async embed(
    contents: readonly Content[],
    options: EmbeddingOptions = {},
  ): Promise<EmbeddingResult> {
    this.validateContents(contents);
    const result = await this.doEmbed(contents, {
      purpose: options.purpose ?? "document",
      signal: options.signal,
    });
    this.validateResult(contents, result);
    return result;
  }

  async dispose(): Promise<void> {
    // Most embedding backends do not hold local resources.
  }

  protected abstract doEmbed(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingResult>;

  private validateContents(contents: readonly Content[]): void {
    if (contents.length === 0) {
      throw new EngineError("Embedding requires at least one content item", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_EMPTY_INPUT",
      });
    }

    if (contents.length > this.info.limits.maxBatchSize) {
      throw new EngineError("Embedding batch size exceeds model limit", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_BATCH_TOO_LARGE",
        context: `model=${this.info.reference} batchSize=${contents.length} maxBatchSize=${this.info.limits.maxBatchSize}`,
      });
    }

    for (const [index, content] of contents.entries()) {
      if (!this.info.inputKinds.includes(content.kind)) {
        throw new EngineError("Embedding model does not support content kind", {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_UNSUPPORTED_CONTENT",
          context: `model=${this.info.reference} index=${index} kind=${content.kind}`,
        });
      }

      if (content.kind === "text" && content.text.trim().length === 0) {
        throw new EngineError("Embedding text content must not be empty", {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_EMPTY_TEXT",
          context: `model=${this.info.reference} index=${index}`,
        });
      }

      if (content.kind === "image") {
        if (content.data.byteLength === 0) {
          throw new EngineError("Embedding image content must not be empty", {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_EMPTY_IMAGE",
            context: `model=${this.info.reference} index=${index}`,
          });
        }

        if (content.format.trim().length === 0) {
          throw new EngineError(
            "Embedding image content must include a format",
            {
              code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_MISSING_IMAGE_FORMAT",
              context: `model=${this.info.reference} index=${index}`,
            },
          );
        }

        const maxImageBytes = this.info.limits.maxImageBytes;
        if (
          maxImageBytes !== undefined &&
          content.data.byteLength > maxImageBytes
        ) {
          throw new EngineError("Embedding image content exceeds model limit", {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_IMAGE_TOO_LARGE",
            context: `model=${this.info.reference} index=${index} imageBytes=${content.data.byteLength} maxImageBytes=${maxImageBytes}`,
          });
        }
      }
    }
  }

  private validateResult(
    contents: readonly Content[],
    result: EmbeddingResult,
  ): void {
    if (!result || !Array.isArray(result.vectors)) {
      throw new EngineError("Embedding model returned a non-array response", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_RESPONSE",
        context: `model=${this.info.reference}`,
      });
    }

    if (result.vectors.length !== contents.length) {
      throw new EngineError(
        "Embedding model returned the wrong number of vectors",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_VECTOR_COUNT_MISMATCH",
          context: `model=${this.info.reference} contentCount=${contents.length} vectorCount=${result.vectors.length}`,
        },
      );
    }

    for (const [vectorIndex, vector] of result.vectors.entries()) {
      if (!Array.isArray(vector)) {
        throw new EngineError("Embedding model returned a non-array vector", {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_VECTOR",
          context: `model=${this.info.reference} vectorIndex=${vectorIndex}`,
        });
      }

      if (vector.length !== this.info.dimension) {
        throw new EngineError(
          "Embedding model returned a vector with the wrong dimension",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_DIMENSION_MISMATCH",
            context: `model=${this.info.reference} vectorIndex=${vectorIndex} expectedDimension=${this.info.dimension} actualDimension=${vector.length}`,
          },
        );
      }

      for (const [valueIndex, value] of vector.entries()) {
        if (!Number.isFinite(value)) {
          throw new EngineError(
            "Embedding model returned a non-finite vector value",
            {
              code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_NON_FINITE_VECTOR_VALUE",
              context: `model=${this.info.reference} vectorIndex=${vectorIndex} valueIndex=${valueIndex}`,
            },
          );
        }
      }
    }

    if (!Array.isArray(result.truncated)) {
      throw new EngineError(
        "Embedding model returned invalid truncation information",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_TRUNCATION",
          context: `model=${this.info.reference}`,
        },
      );
    }

    const seen = new Set<number>();
    for (const index of result.truncated) {
      if (
        !Number.isInteger(index) ||
        index < 0 ||
        index >= contents.length ||
        seen.has(index)
      ) {
        throw new EngineError(
          "Embedding model returned an invalid truncated input index",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_TRUNCATED_INPUT_INDEX",
            context: `model=${this.info.reference} index=${index} inputCount=${contents.length}`,
          },
        );
      }
      seen.add(index);
    }
  }
}
