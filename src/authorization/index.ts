export { RemoteEmbeddingAuthorizationManager } from "./manager.js";
export {
  createRemoteEmbeddingOperationPermit,
  remoteEmbeddingAuthorizationGuard,
  withRemoteEmbeddingOperationPermit,
} from "./operation.js";
export { RemoteEmbeddingAuthorizationStore } from "./store.js";
export {
  formatRemoteEmbeddingAuthorizationPrompt,
  remoteEmbeddingDisclosureData,
  type RemoteEmbeddingAuthorizationPromptInput,
} from "./prompt.js";
export {
  indexStatusIsFresh,
  planRemoteIndexAuthorization,
  planRemoteSearchAuthorization,
  searchUsesVector,
} from "./planner.js";
export {
  canonicalizeWorkspaceRoots,
  createRemoteEmbeddingTarget,
  remoteEmbeddingTargetFingerprint,
  workspaceFingerprint,
} from "./target.js";
export type {
  RemoteEmbeddingAuthorizationPlan,
  RemoteEmbeddingAuthorizationScope,
  RemoteEmbeddingAuthorizationStatus,
  RemoteEmbeddingDataDisclosure,
  RemoteEmbeddingOperation,
  RemoteEmbeddingOperationPermit,
  RemoteEmbeddingTarget,
  RemoteEmbeddingWorkspaceGrant,
} from "./types.js";
