export const EMBEDDING_ENVIRONMENT_META_KEY =
  "io.zvec-grep/embedding-environment";

export function embeddingEnvironmentFromRequestMeta(
  meta: unknown,
): string | undefined {
  if (typeof meta !== "object" || meta === null) return undefined;
  const value = (meta as Record<string, unknown>)[
    EMBEDDING_ENVIRONMENT_META_KEY
  ];
  if (typeof value !== "string") return undefined;
  const normalized = value.trim();
  return normalized.length > 0 ? normalized : undefined;
}
