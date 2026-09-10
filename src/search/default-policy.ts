import type { NormalizedSearchInput } from "../mcp/input-normalization.js";
import type { ZvecGrepInfoResult } from "../engine/service/types.js";

/** Current-source fallback must not reinterpret explicitly requested routes. */
export function canUseCurrentSource(input: NormalizedSearchInput): boolean {
  return (
    input.queries?.length === 1 &&
    input.queries[0]!.trim().length > 0 &&
    input.routes.length === 0 &&
    !input.fuse &&
    !input.preferSymbol &&
    !input.symbolTypes?.length &&
    !input.trace &&
    input.freshness === "eventual" &&
    input.semanticPolicy !== "wait"
  );
}

/** Metadata about a populated index keeps warm retrieval on its normal path. */
export function isLocalIndexUnavailable(
  info: ZvecGrepInfoResult,
  activeProvider?: string,
): boolean {
  if (activeProvider && activeProvider !== "local") return false;
  const provider = info.workspaceIndex?.embedding?.provider ?? activeProvider;
  if (provider && provider !== "local") return false;
  if (!info.indexed || info.indexPolicy === "disabled") return true;
  const status = info.status;
  return (
    status?.filesIndexed === 0 &&
    (status.filesAdded > 0 || status.filesPending > 0 || status.filesFailed > 0)
  );
}
