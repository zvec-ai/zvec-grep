import { EngineError } from "../errors/index.js";
import type { Content, ContentKind } from "../types.js";
import type { ModelRef, VectorMetric } from "./types.js";

export type EmbeddingVector = number[];

export type EmbeddingDiagnostics = {
  truncatedInputIndexes: number[];
};

export type EmbeddingBatchResult = {
  vectors: EmbeddingVector[];
  diagnostics: EmbeddingDiagnostics;
};

export type EmbeddingPurpose = "document" | "query";

export type EmbeddingOptions = {
  purpose?: EmbeddingPurpose;
  signal?: AbortSignal;
};

export type NormalizedEmbeddingOptions = {
  purpose: EmbeddingPurpose;
  signal?: AbortSignal;
};

export type EmbeddingLimits = {
  maxBatchSize: number;
  maxInputTokens?: number;
  maxImageBytes?: number;
};

export abstract class EmbeddingModel {
  abstract readonly ref: ModelRef;
  abstract readonly dimension: number;
  abstract readonly metric: VectorMetric;
  abstract readonly supportedContentKinds: readonly ContentKind[];
  abstract readonly limits: EmbeddingLimits;
  readonly recommendedIndexConcurrency?: number;
  readonly maxIndexConcurrency?: number;

  async embed(
    contents: readonly Content[],
    options: EmbeddingOptions = {},
  ): Promise<EmbeddingVector[]> {
    return (await this.embedWithDiagnostics(contents, options)).vectors;
  }

  async embedWithDiagnostics(
    contents: readonly Content[],
    options: EmbeddingOptions = {},
  ): Promise<EmbeddingBatchResult> {
    this.validateContents(contents);
    const result = await this.doEmbedWithDiagnostics(contents, {
      purpose: options.purpose ?? "document",
      signal: options.signal,
    });
    this.validateVectors(contents, result.vectors);
    this.validateDiagnostics(contents, result.diagnostics);

    return result;
  }

  async dispose(): Promise<void> {
    // Most embedding providers do not hold local resources.
  }

  protected abstract doEmbed(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingVector[]>;

  protected async doEmbedWithDiagnostics(
    contents: readonly Content[],
    options: NormalizedEmbeddingOptions,
  ): Promise<EmbeddingBatchResult> {
    return {
      vectors: await this.doEmbed(contents, options),
      diagnostics: { truncatedInputIndexes: [] },
    };
  }

  private validateContents(contents: readonly Content[]): void {
    if (contents.length === 0) {
      throw new EngineError("Embedding requires at least one content item", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_EMPTY_INPUT",
      });
    }

    if (contents.length > this.limits.maxBatchSize) {
      throw new EngineError("Embedding batch size exceeds model limit", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_BATCH_TOO_LARGE",
        context: `model=${this.ref.model} batchSize=${contents.length} maxBatchSize=${this.limits.maxBatchSize}`,
      });
    }

    for (const [index, content] of contents.entries()) {
      if (!this.supportedContentKinds.includes(content.kind)) {
        throw new EngineError("Embedding model does not support content kind", {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_UNSUPPORTED_CONTENT",
          context: `model=${this.ref.model} index=${index} kind=${content.kind}`,
        });
      }

      if (content.kind === "text" && content.text.trim().length === 0) {
        throw new EngineError("Embedding text content must not be empty", {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_EMPTY_TEXT",
          context: `model=${this.ref.model} index=${index}`,
        });
      }

      if (content.kind === "image") {
        if (content.data.byteLength === 0) {
          throw new EngineError("Embedding image content must not be empty", {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_EMPTY_IMAGE",
            context: `model=${this.ref.model} index=${index}`,
          });
        }

        if (content.format.trim().length === 0) {
          throw new EngineError(
            "Embedding image content must include a format",
            {
              code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_MISSING_IMAGE_FORMAT",
              context: `model=${this.ref.model} index=${index}`,
            },
          );
        }

        if (
          this.limits.maxImageBytes !== undefined &&
          content.data.byteLength > this.limits.maxImageBytes
        ) {
          throw new EngineError("Embedding image content exceeds model limit", {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_IMAGE_TOO_LARGE",
            context: `model=${this.ref.model} index=${index} imageBytes=${content.data.byteLength} maxImageBytes=${this.limits.maxImageBytes}`,
          });
        }
      }
    }
  }

  private validateVectors(
    contents: readonly Content[],
    vectors: EmbeddingVector[],
  ): void {
    if (!Array.isArray(vectors)) {
      throw new EngineError("Embedding model returned a non-array response", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_RESPONSE",
        context: `model=${this.ref.model}`,
      });
    }

    if (vectors.length !== contents.length) {
      throw new EngineError(
        "Embedding model returned the wrong number of vectors",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_VECTOR_COUNT_MISMATCH",
          context: `model=${this.ref.model} contentCount=${contents.length} vectorCount=${vectors.length}`,
        },
      );
    }

    for (const [vectorIndex, vector] of vectors.entries()) {
      if (!Array.isArray(vector)) {
        throw new EngineError("Embedding model returned a non-array vector", {
          code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_VECTOR",
          context: `model=${this.ref.model} vectorIndex=${vectorIndex}`,
        });
      }

      if (vector.length !== this.dimension) {
        throw new EngineError(
          "Embedding model returned a vector with the wrong dimension",
          {
            code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_DIMENSION_MISMATCH",
            context: `model=${this.ref.model} vectorIndex=${vectorIndex} expectedDimension=${this.dimension} actualDimension=${vector.length}`,
          },
        );
      }

      for (const [valueIndex, value] of vector.entries()) {
        if (!Number.isFinite(value)) {
          throw new EngineError(
            "Embedding model returned a non-finite vector value",
            {
              code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_NON_FINITE_VECTOR_VALUE",
              context: `model=${this.ref.model} vectorIndex=${vectorIndex} valueIndex=${valueIndex}`,
            },
          );
        }
      }
    }
  }

  private validateDiagnostics(
    contents: readonly Content[],
    diagnostics: EmbeddingDiagnostics,
  ): void {
    if (!diagnostics || !Array.isArray(diagnostics.truncatedInputIndexes)) {
      throw new EngineError("Embedding model returned invalid diagnostics", {
        code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_DIAGNOSTICS",
        context: `model=${this.ref.model}`,
      });
    }

    const seen = new Set<number>();
    for (const index of diagnostics.truncatedInputIndexes) {
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
            context: `model=${this.ref.model} index=${index} inputCount=${contents.length}`,
          },
        );
      }
      seen.add(index);
    }
  }
}
