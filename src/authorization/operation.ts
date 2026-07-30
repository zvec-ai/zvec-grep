import { AsyncLocalStorage } from "node:async_hooks";
import { randomUUID } from "node:crypto";
import { EngineError } from "../engine/errors/index.js";
import { RemoteEmbeddingAuthorizationStore } from "./store.js";
import {
  REMOTE_EMBEDDING_CAPABILITY,
  type RemoteEmbeddingAuthorizationScope,
  type RemoteEmbeddingOperationPermit,
  type RemoteEmbeddingRequest,
  type RemoteEmbeddingTarget,
} from "./types.js";

const operationPermit = new AsyncLocalStorage<RemoteEmbeddingOperationPermit>();

export function createRemoteEmbeddingOperationPermit(
  target: RemoteEmbeddingTarget,
  scope: RemoteEmbeddingAuthorizationScope,
): RemoteEmbeddingOperationPermit {
  return {
    capability: REMOTE_EMBEDDING_CAPABILITY,
    scope,
    target,
    issuedAt: Date.now(),
    operationId: randomUUID(),
  };
}

export async function withRemoteEmbeddingOperationPermit<T>(
  permit: RemoteEmbeddingOperationPermit | undefined,
  operation: () => Promise<T>,
): Promise<T> {
  if (!permit) return await operation();
  return await operationPermit.run(permit, operation);
}

export function remoteEmbeddingAuthorizationGuard(
  options: {
    store?: RemoteEmbeddingAuthorizationStore;
  } = {},
): (request: RemoteEmbeddingRequest) => Promise<void> {
  const store = options.store ?? new RemoteEmbeddingAuthorizationStore();
  return async (request) => {
    const permit = operationPermit.getStore();
    if (
      !permit ||
      permit.capability !== REMOTE_EMBEDDING_CAPABILITY ||
      permit.target.provider !== request.provider ||
      permit.target.model !== request.model ||
      permit.target.endpoint !== request.endpoint
    ) {
      throw authorizationRequiredError(request);
    }
    if (
      permit.scope === "workspace" &&
      !(await store.hasGrant(permit.target))
    ) {
      throw authorizationRequiredError(
        request,
        "Workspace grant is missing, invalid, or revoked.",
      );
    }
  };
}

function authorizationRequiredError(
  request: RemoteEmbeddingRequest,
  detail?: string,
): EngineError {
  return new EngineError("Remote Embedding authorization is required", {
    code: "ZVEC_GREP.ENGINE.AUTH.REMOTE_EMBEDDING_REQUIRED",
    context: [
      `provider=${request.provider}`,
      `model=${request.model}`,
      `endpoint=${request.endpoint}`,
      `purpose=${request.purpose}`,
      ...(detail ? [`detail=${detail}`] : []),
    ].join(" "),
  });
}
