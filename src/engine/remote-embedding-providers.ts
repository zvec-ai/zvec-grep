export const REMOTE_EMBEDDING_PROVIDER_CATALOG = {
  qwen: {
    provider: "qwen",
    apiKey: "required",
  },
  dgx: {
    provider: "dgx",
    apiKey: "optional",
  },
} as const;

export type RemoteEmbeddingProviderCatalogEntry =
  (typeof REMOTE_EMBEDDING_PROVIDER_CATALOG)[keyof typeof REMOTE_EMBEDDING_PROVIDER_CATALOG];

export function listRemoteEmbeddingProviders(): RemoteEmbeddingProviderCatalogEntry[] {
  return Object.values(REMOTE_EMBEDDING_PROVIDER_CATALOG);
}

export function getRemoteEmbeddingProviderCatalogEntry(
  provider: string,
): RemoteEmbeddingProviderCatalogEntry | undefined {
  return REMOTE_EMBEDDING_PROVIDER_CATALOG[
    provider as keyof typeof REMOTE_EMBEDDING_PROVIDER_CATALOG
  ];
}

export function isRemoteEmbeddingProvider(provider: string): boolean {
  return provider !== "local";
}
