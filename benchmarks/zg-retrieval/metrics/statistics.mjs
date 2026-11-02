/** Nearest-rank percentiles; an empty population is unavailable, never zero. */
export function statistics(values) {
  const sorted = values
    .filter((value) => Number.isFinite(value) && value >= 0)
    .sort((a, b) => a - b);
  const count = sorted.length;
  const total = sorted.reduce((sum, value) => sum + value, 0);
  const rank = (p) => (count ? sorted[Math.ceil(p * count) - 1] : null);
  return {
    count,
    mean: count ? total / count : null,
    p50: count
      ? (sorted[Math.floor((count - 1) / 2)] + sorted[Math.floor(count / 2)]) /
        2
      : null,
    p95: rank(0.95),
    p99: rank(0.99),
    max: count ? sorted[count - 1] : null,
    total: count ? total : null,
  };
}
