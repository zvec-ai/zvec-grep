import type { LocalEdge } from "./types.js";
import type { LocalReferenceLookupPlan } from "./reference-resolution-policy.js";

export type LocalReferenceIndex = {
  nameToIds: ReadonlyMap<string, readonly string[]>;
  containerIdByChild: ReadonlyMap<string, string>;
  localEdges: ReadonlyMap<string, LocalEdge>;
};

/** Resolve a structured target against symbols and hierarchy in one file. */
export function resolveLocalReferenceCandidates(
  plan: LocalReferenceLookupPlan,
  ownerId: string,
  index: LocalReferenceIndex,
): string[] {
  const candidates = index.nameToIds.get(plan.lookupName) ?? [];
  if (plan.containerScope.kind === "none") return [...candidates];
  const containerIds = localContainerScope(plan.containerScope, ownerId, index);
  for (const containerId of containerIds) {
    const hits = candidates.filter(
      (candidate) => index.containerIdByChild.get(candidate) === containerId,
    );
    if (hits.length > 0) return hits;
  }
  return [];
}

function localContainerScope(
  scope: LocalReferenceLookupPlan["containerScope"],
  ownerId: string,
  index: LocalReferenceIndex,
): string[] {
  if (scope.kind === "named") {
    return [...(index.nameToIds.get(scope.name) ?? [])];
  }
  if (scope.kind !== "owner-hierarchy") return [];
  const ownerContainer = index.containerIdByChild.get(ownerId);
  if (!ownerContainer) return [];
  const seen = new Set<string>();
  const containers: string[] = [];
  const queue = scope.includeOwner
    ? [ownerContainer]
    : localBases(ownerContainer);
  while (queue.length > 0) {
    const current = queue.shift()!;
    if (seen.has(current)) continue;
    seen.add(current);
    containers.push(current);
    queue.push(...localBases(current));
  }
  return containers;

  function localBases(containerId: string): string[] {
    return [...index.localEdges.values()]
      .filter(
        (edge) =>
          edge.kind === "INHERITS" &&
          (edge.rel === "extends" || edge.rel === "implements") &&
          edge.src === containerId,
      )
      .map((edge) => edge.dst);
  }
}
