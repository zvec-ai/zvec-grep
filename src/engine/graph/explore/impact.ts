import type { GraphQueryStorage } from "../ports.js";
import type { GraphReader } from "../types.js";
import type {
  ExploreBlastRadius,
  ExploreChangeSurfaceRef,
  ExploreImpactRef,
  ExploreNode,
} from "./types.js";
import { isTestPath, isTypeishKind, queryTerms, symbolName } from "./policy.js";

export function collectBlastRadius(
  graph: GraphReader,
  storage: GraphQueryStorage,
  rootIds: readonly string[],
  limit: number,
): ExploreBlastRadius[] {
  return rootIds.map((rootId) => {
    const refs = graph.impact(rootId, 3, limit * 3);
    const dependents: ExploreImpactRef[] = [];
    const tests: ExploreImpactRef[] = [];
    const seen = new Set<string>();
    for (const ref of refs) {
      if (seen.has(ref.id)) {
        continue;
      }
      seen.add(ref.id);
      const item = { id: ref.id, entity: storage.getEntity(ref.id) };
      if (item.entity && isTestPath(item.entity.file.relativePath)) {
        if (tests.length < limit) tests.push(item);
      } else if (dependents.length < limit) {
        dependents.push(item);
      }
    }
    return { rootId, dependents, tests };
  });
}

export function collectChangeSurface(input: {
  graph: GraphReader;
  storage: GraphQueryStorage;
  rootIds: readonly string[];
  nodes: readonly ExploreNode[];
  nodeScores: ReadonlyMap<string, number>;
  fileScores: Map<string, number>;
  query: string;
  maxFiles: number;
}): ExploreChangeSurfaceRef[] {
  const callableRoots = input.rootIds.filter((id) => {
    const entity = input.storage.getEntity(id);
    const kind =
      entity?.entity.metadata?.kind === "code"
        ? entity.entity.metadata.symbolType
        : "";
    // Extractors normalize free functions, methods and constructors to function.
    return kind === "function";
  });
  const candidates: Omit<ExploreChangeSurfaceRef, "rescued">[] = [];
  const seen = new Set<string>();
  for (const rootId of callableRoots.slice(0, 5)) {
    for (const ref of input.graph.context(rootId).outgoing) {
      if (ref.rel !== "type" && ref.rel !== "return") continue;
      const entity = input.storage.getEntity(ref.id);
      const kind =
        entity?.entity.metadata?.kind === "code"
          ? entity.entity.metadata.symbolType
          : "";
      if (!entity || !isTypeishKind(kind)) continue;
      const key = `${rootId}\0${ref.id}\0${ref.rel}`;
      if (seen.has(key)) continue;
      seen.add(key);
      candidates.push({ rootId, id: ref.id, rel: ref.rel, entity });
    }
  }

  const rankedFileIds = [...input.fileScores.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, input.maxFiles)
    .map(([id]) => id);
  const visibleFiles = new Set(rankedFileIds);
  const maxFileScore = Math.max(...input.fileScores.values(), 0);
  const maxNodeScore = Math.max(...input.nodeScores.values(), 0);
  const terms = queryTerms(input.query);

  return candidates.map((candidate) => {
    const fileId = candidate.entity.file.id;
    const fileScore = input.fileScores.get(fileId) ?? 0;
    const nodeScore = input.nodeScores.get(candidate.id) ?? 0;
    const hay =
      `${symbolName(candidate.entity)} ${candidate.entity.file.relativePath}`.toLowerCase();
    const weakText = terms.every((term) => !hay.includes(term));
    const weakGraph =
      !visibleFiles.has(fileId) ||
      (fileScore < maxFileScore * 0.06 && nodeScore < maxNodeScore * 0.06);
    const rescued = weakText && weakGraph;
    if (rescued && !input.fileScores.has(fileId)) {
      input.fileScores.set(fileId, 0);
    }
    return { ...candidate, rescued };
  });
}

export function includeChangeSurfaceNodes(
  nodes: readonly ExploreNode[],
  changeSurface: readonly ExploreChangeSurfaceRef[],
): ExploreNode[] {
  const out = [...nodes];
  const seen = new Set(nodes.map((node) => node.id));
  for (const item of changeSurface) {
    if (!item.rescued || seen.has(item.id)) continue;
    seen.add(item.id);
    out.push({
      id: item.id,
      kind:
        item.entity.entity.metadata?.kind === "code"
          ? item.entity.entity.metadata.symbolType
          : undefined,
      isRoot: false,
      entity: item.entity,
    });
  }
  return out;
}

export function fileIdsForRoots(
  nodes: readonly ExploreNode[],
  rootIds: readonly string[],
): Set<string> {
  const roots = new Set(rootIds);
  return new Set(
    nodes
      .filter((node) => roots.has(node.id))
      .map((node) => node.entity?.file.id)
      .filter((id): id is string => Boolean(id)),
  );
}
