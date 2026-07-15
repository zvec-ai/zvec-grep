import { EngineError } from "../errors/index.js";
import {
  getEmbeddingModelCatalogEntry,
  getEmbeddingModelCatalogEntryByRef,
} from "./catalog.js";
import type { EmbeddingModel } from "./embeddings.js";
import {
  LlamaCppEmbeddingModel,
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
} from "./providers/index.js";
import type { ModelProviderOptions, ModelRef } from "./types.js";

export function createEmbeddingModel(
  ref: ModelRef,
  options: ModelProviderOptions,
): EmbeddingModel {
  if (ref.provider === "local") {
    const entry = getEmbeddingModelCatalogEntryByRef(ref);
    if (!entry || entry.provider !== "local") {
      throw new EngineError(
        "Local embedding model is not in the zvec-grep catalog",
        {
          code: "ZVEC_GREP.ENGINE.MODELS.LOCAL_MODEL_NOT_IN_CATALOG",
          context: `provider=${ref.provider} model=${ref.model}`,
        },
      );
    }

    return new LlamaCppEmbeddingModel(entry, options);
  }

  if (ref.provider === "qwen" && ref.model === "text-embedding-v4") {
    return new QwenTextEmbeddingV4Model(options);
  }

  if (ref.provider === "qwen" && ref.model === "qwen3-vl-embedding") {
    return new Qwen3VlEmbeddingModel(options);
  }

  throw new EngineError("Embedding model is not implemented", {
    code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_MODEL_NOT_IMPLEMENTED",
    context: `provider=${ref.provider} model=${ref.model}`,
  });
}

export function createEmbeddingModelFromCatalog(
  id: string,
  options: ModelProviderOptions,
): EmbeddingModel {
  const entry = getEmbeddingModelCatalogEntry(id);
  if (!entry) {
    throw new EngineError("Embedding model is not in the zvec-grep catalog", {
      code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_CATALOG_MODEL_NOT_FOUND",
      context: `embedding=${id}`,
    });
  }

  return createEmbeddingModel(
    {
      provider: entry.provider,
      model: entry.model,
    },
    options,
  );
}

export function createEmbeddingModelFromReference(
  reference: string,
  options: ModelProviderOptions,
): EmbeddingModel {
  const catalogEntry = getEmbeddingModelCatalogEntry(reference);
  if (catalogEntry) {
    return createEmbeddingModel(
      {
        provider: catalogEntry.provider,
        model: catalogEntry.model,
      },
      options,
    );
  }

  const ref = parseModelReference(reference);
  if (!ref) {
    throw new EngineError("Embedding model is not in the zvec-grep catalog", {
      code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_CATALOG_MODEL_NOT_FOUND",
      context: `embedding=${reference}`,
    });
  }

  return createEmbeddingModel(ref, options);
}

function parseModelReference(reference: string): ModelRef | undefined {
  const separator = reference.indexOf("/");
  if (separator <= 0 || separator === reference.length - 1) {
    return undefined;
  }

  return {
    provider: reference.slice(0, separator),
    model: reference.slice(separator + 1),
  };
}
