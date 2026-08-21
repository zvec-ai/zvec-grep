const DEFAULT_RESTART_PROBABILITY = 0.25;
const DEFAULT_ITERATIONS = 25;

export function personalizedPageRank(
  nodeIds: readonly string[],
  adjacency: Map<string, Map<string, number>>,
  seedIds: readonly string[],
  seedWeights?: ReadonlyMap<string, number>,
  options: { restartProbability?: number; iterations?: number } = {},
): Map<string, number> {
  const scores = new Map<string, number>();
  if (nodeIds.length === 0) return scores;
  const seeds = seedIds.length > 0 ? seedIds : nodeIds.slice(0, 1);
  const restart = normalizedSeedWeights(seeds, seedWeights);
  const alpha = options.restartProbability ?? DEFAULT_RESTART_PROBABILITY;
  const iterations = options.iterations ?? DEFAULT_ITERATIONS;
  for (const id of nodeIds) scores.set(id, restart.get(id) ?? 0);
  for (let iter = 0; iter < iterations; iter++) {
    const next = new Map(
      nodeIds.map((id) => [id, alpha * (restart.get(id) ?? 0)]),
    );
    for (const id of nodeIds) {
      const neighbors = [...(adjacency.get(id)?.entries() ?? [])];
      const mass = (scores.get(id) ?? 0) * (1 - alpha);
      if (neighbors.length === 0) {
        for (const seed of seeds) {
          next.set(
            seed,
            (next.get(seed) ?? 0) + mass * (restart.get(seed) ?? 0),
          );
        }
        continue;
      }
      const total = neighbors.reduce((sum, [, weight]) => sum + weight, 0);
      for (const [neighbor, weight] of neighbors) {
        next.set(
          neighbor,
          (next.get(neighbor) ?? 0) + (total > 0 ? (mass * weight) / total : 0),
        );
      }
    }
    for (const id of nodeIds) scores.set(id, next.get(id) ?? 0);
  }
  return scores;
}

function normalizedSeedWeights(
  seedIds: readonly string[],
  weights?: ReadonlyMap<string, number>,
): Map<string, number> {
  const raw = seedIds.map((id) => ({
    id,
    weight: Math.max(0, weights?.get(id) ?? 1),
  }));
  const total = raw.reduce((sum, item) => sum + item.weight, 0);
  const fallback = 1 / Math.max(1, seedIds.length);
  return new Map(
    raw.map((item) => [item.id, total > 0 ? item.weight / total : fallback]),
  );
}
