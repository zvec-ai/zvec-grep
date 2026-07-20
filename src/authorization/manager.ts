import { createRemoteEmbeddingOperationPermit } from "./operation.js";
import { RemoteEmbeddingAuthorizationStore } from "./store.js";
import type {
  RemoteEmbeddingAuthorizationPlan,
  RemoteEmbeddingAuthorizationScope,
  RemoteEmbeddingOperationPermit,
} from "./types.js";

export class RemoteEmbeddingAuthorizationManager {
  readonly store: RemoteEmbeddingAuthorizationStore;

  constructor(store = new RemoteEmbeddingAuthorizationStore()) {
    this.store = store;
  }

  async existingWorkspacePermit(
    plan: RemoteEmbeddingAuthorizationPlan,
  ): Promise<RemoteEmbeddingOperationPermit | undefined> {
    if (!(await this.store.hasGrant(plan.target))) return undefined;
    return createRemoteEmbeddingOperationPermit(plan.target, "workspace");
  }

  async grant(
    plan: RemoteEmbeddingAuthorizationPlan,
    scope: RemoteEmbeddingAuthorizationScope,
  ): Promise<RemoteEmbeddingOperationPermit> {
    if (scope === "workspace") await this.store.grant(plan.target);
    return createRemoteEmbeddingOperationPermit(plan.target, scope);
  }
}
