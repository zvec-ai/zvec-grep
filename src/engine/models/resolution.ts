import { EngineError } from "../errors.js";
import { getEmbeddingModelCatalogEntry } from "./catalog.js";

export type ResolveEmbeddingReferenceOptions = {
  explicit?: string;
  existing?: string;
  globalDefault?: string;
  environment?: NodeJS.ProcessEnv;
  fallback?: string;
};

export function resolveEmbeddingReference(
  options: ResolveEmbeddingReferenceOptions,
): string | undefined {
  if (options.explicit !== undefined) return options.explicit;
  if (options.existing !== undefined) return options.existing;

  const environmentReference = nonEmptyEnvironmentValue(
    (options.environment ?? process.env).ZVEC_GREP_EMBEDDING,
  );
  if (
    environmentReference !== undefined &&
    !getEmbeddingModelCatalogEntry(environmentReference)
  ) {
    throw new EngineError(
      `Invalid ZVEC_GREP_EMBEDDING: unsupported model ${environmentReference}. Run \`zg help models\` to list supported models.`,
      {
        code: "ZVEC_GREP.ENGINE.CONFIG.EMBEDDING_ENVIRONMENT_INVALID",
        context: "source=ZVEC_GREP_EMBEDDING",
      },
    );
  }
  return environmentReference ?? options.globalDefault ?? options.fallback;
}

function nonEmptyEnvironmentValue(
  value: string | undefined,
): string | undefined {
  const normalized = value?.trim();
  return normalized ? normalized : undefined;
}
