import type { GraphReader, SymRef } from "../types.js";
import type { ExploreCallPath } from "./types.js";

const MAX_PATH_SEEDS = 8;
const MAX_PATH_ATTEMPTS = 32;
const MAX_PATH_EDGE_READS = 20_000;

export function collectCallPaths(
  graph: GraphReader,
  rootIds: readonly string[],
  maxDepth: number,
  limit: number,
): { paths: ExploreCallPath[]; refs: SymRef[] } {
  const paths: ExploreCallPath[] = [];
  const refs: SymRef[] = [];
  const seen = new Set<string>();
  const pathSeeds = rootIds.slice(0, MAX_PATH_SEEDS);
  let attempts = 0;
  let edgeReadsRemaining = MAX_PATH_EDGE_READS;
  const tryPath = (from: string, to: string): SymRef[] | null => {
    if (attempts >= MAX_PATH_ATTEMPTS || edgeReadsRemaining <= 0) return null;
    const allowance = Math.max(
      1,
      Math.floor(edgeReadsRemaining / (MAX_PATH_ATTEMPTS - attempts)),
    );
    attempts += 1;
    edgeReadsRemaining -= allowance;
    return graph.pathBetween(from, to, maxDepth, allowance);
  };
  for (let i = 0; i < pathSeeds.length && paths.length < limit; i++) {
    for (let j = i + 1; j < pathSeeds.length && paths.length < limit; j++) {
      if (attempts >= MAX_PATH_ATTEMPTS || edgeReadsRemaining <= 0) break;
      const forward = tryPath(pathSeeds[i]!, pathSeeds[j]!);
      const path = forward ?? tryPath(pathSeeds[j]!, pathSeeds[i]!);
      if (!path || path.length < 2) continue;
      const ids = path.map((ref) => ref.id);
      const key = ids.join("\0");
      if (seen.has(key)) continue;
      seen.add(key);
      refs.push(...path);
      paths.push({ from: ids[0]!, to: ids[ids.length - 1]!, nodes: ids });
    }
  }
  return { paths, refs };
}
