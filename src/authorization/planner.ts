import type { NormalizedSearchInput } from "../mcp/input-normalization.js";
import type { EmbeddingModelInfo } from "../engine/models/index.js";
import type { ZvecGrepInfoResult } from "../engine/service/types.js";
import { createRemoteEmbeddingTarget } from "./target.js";
import { RemoteEmbeddingAuthorizationStore } from "./store.js";
import type { RemoteEmbeddingAuthorizationPlan } from "./types.js";

export async function planRemoteIndexAuthorization(input: {
  info: ZvecGrepInfoResult;
  model: EmbeddingModelInfo;
  rebuild?: boolean;
  needsUpdate?: boolean;
  store?: RemoteEmbeddingAuthorizationStore;
}): Promise<RemoteEmbeddingAuthorizationPlan | undefined> {
  if (input.model.provider !== "qwen") return undefined;
  const needsEmbedding =
    input.rebuild === true ||
    input.needsUpdate === true ||
    !input.info.indexed ||
    !indexStatusIsFresh(input.info);
  if (!needsEmbedding) return undefined;
  const endpoint = input.model.endpoint;
  if (endpoint === undefined) {
    throw new Error(
      `Embedding model ${input.model.reference} did not provide a remote endpoint.`,
    );
  }
  const target = await createRemoteEmbeddingTarget({
    roots: workspaceRoots(input.info),
    provider: input.model.provider,
    model: input.model.name,
    endpoint,
  });
  const store = input.store ?? new RemoteEmbeddingAuthorizationStore();
  return {
    operation: "index",
    target,
    disclosure: {
      queryText: false,
      workspaceContent:
        !input.info.indexed || input.rebuild ? "full" : "changed",
    },
    reason: input.rebuild
      ? "index_rebuild"
      : input.info.indexed
        ? "index_update"
        : "index_create",
    grantPath: store.grantPath(target),
  };
}

export async function planRemoteSearchAuthorization(input: {
  info: ZvecGrepInfoResult;
  model: EmbeddingModelInfo;
  search: NormalizedSearchInput;
  runtimeNeedsReconciliation?: boolean;
  store?: RemoteEmbeddingAuthorizationStore;
}): Promise<RemoteEmbeddingAuthorizationPlan | undefined> {
  const schema = input.info.collection?.embedding;
  if (!input.info.indexed || !schema || schema.provider !== "qwen") {
    return undefined;
  }
  if (
    input.model.provider !== schema.provider ||
    input.model.name !== schema.model
  ) {
    throw new Error(
      `Embedding model ${input.model.reference} does not match indexed model ${schema.provider}/${schema.model}.`,
    );
  }
  const usesVector = searchUsesVector(input.search);
  const needsUpdate =
    input.runtimeNeedsReconciliation === true ||
    !indexStatusIsFresh(input.info);
  const updatesIndex =
    needsUpdate &&
    (input.search.autoUpdate || input.search.freshness === "wait_for_fresh");
  if (!usesVector && !updatesIndex) return undefined;
  const endpoint = input.model.endpoint;
  if (endpoint === undefined) {
    throw new Error(
      `Embedding model ${input.model.reference} did not provide a remote endpoint.`,
    );
  }
  const target = await createRemoteEmbeddingTarget({
    roots: workspaceRoots(input.info),
    provider: input.model.provider,
    model: input.model.name,
    endpoint,
  });
  const store = input.store ?? new RemoteEmbeddingAuthorizationStore();
  return {
    operation: usesVector
      ? updatesIndex
        ? "query_and_index"
        : "query"
      : "index",
    target,
    disclosure: {
      queryText: usesVector,
      workspaceContent: updatesIndex ? "changed" : "none",
    },
    reason: "query",
    grantPath: store.grantPath(target),
  };
}

export function searchUsesVector(
  input: Pick<NormalizedSearchInput, "queries" | "routes">,
): boolean {
  return (
    (input.queries?.length ?? 0) > 0 ||
    input.routes.some((route) => route.mode === "vector")
  );
}

export function indexStatusIsFresh(info: ZvecGrepInfoResult): boolean {
  const status = info.status;
  return Boolean(
    status &&
    status.filesAdded === 0 &&
    status.filesModified === 0 &&
    status.filesDeleted === 0 &&
    status.filesPending === 0 &&
    status.filesFailed === 0,
  );
}

function workspaceRoots(info: ZvecGrepInfoResult): string[] {
  const roots =
    info.collection?.rootPaths.map((root) => root.absolutePath) ?? [];
  return roots.length > 0 ? roots : [info.root];
}
