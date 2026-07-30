export const REMOTE_EMBEDDING_CAPABILITY = "remote_embedding" as const;

export type RemoteEmbeddingAuthorizationScope = "once" | "workspace";

export type RemoteEmbeddingOperation = "query" | "index" | "query_and_index";

export type RemoteEmbeddingDataDisclosure = {
  queryText: boolean;
  workspaceContent: "none" | "selected" | "changed" | "full";
};

export type RemoteEmbeddingTarget = {
  workspaceRoots: readonly string[];
  workspaceFingerprint: string;
  provider: string;
  model: string;
  endpoint: string;
  targetFingerprint: string;
};

export type RemoteEmbeddingRequest = {
  provider: string;
  model: string;
  endpoint: string;
  purpose: "document" | "query";
  contentKinds: readonly ("text" | "image")[];
  contentCount: number;
};

export type RemoteEmbeddingAuthorizationPlan = {
  operation: RemoteEmbeddingOperation;
  target: RemoteEmbeddingTarget;
  disclosure: RemoteEmbeddingDataDisclosure;
  reason: "query" | "index_create" | "index_update" | "index_rebuild";
  grantPath: string;
};

export type RemoteEmbeddingOperationPermit = {
  capability: typeof REMOTE_EMBEDDING_CAPABILITY;
  scope: RemoteEmbeddingAuthorizationScope;
  target: RemoteEmbeddingTarget;
  issuedAt: number;
  operationId: string;
};

export type RemoteEmbeddingWorkspaceGrant = {
  version: 1;
  id: string;
  capability: typeof REMOTE_EMBEDDING_CAPABILITY;
  scope: "workspace";
  workspaceRoots: string[];
  workspaceFingerprint: string;
  provider: string;
  model: string;
  endpoint: string;
  targetFingerprint: string;
  grantedAt: number;
  signature: string;
};

export type RemoteEmbeddingAuthorizationDocument = {
  version: 1;
  grants: RemoteEmbeddingWorkspaceGrant[];
};

export type RemoteEmbeddingAuthorizationStatus = {
  path: string;
  grants: Array<
    Omit<RemoteEmbeddingWorkspaceGrant, "signature"> & {
      valid: boolean;
    }
  >;
};
