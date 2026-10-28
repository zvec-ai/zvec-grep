import { summarizePerformance } from "../../zg-retrieval/reports/performance.mjs";
import { statistics } from "../../zg-retrieval/metrics/statistics.mjs";

export const requestFixture = {
  accepted: "true",
  repository: "owner/repo",
  run_id: "123",
  pr_number: "7",
  comment_id: "42",
  baseline_sha: "a".repeat(40),
  candidate_sha: "b".repeat(40),
  harness_commit: "c".repeat(40),
  baseline_repository: "owner/repo",
  candidate_repository: "contributor/repo",
};

export function comparisonFixture() {
  const stage = () => ({
    status: "success",
    concurrency: 4,
    total_ms: 1000,
    memory: {
      status: "available",
      samples: [{ rss_bytes: 1024 }, { rss_bytes: 2048 }],
    },
  });
  const arm = (commit, scale) => ({
    commit,
    status: "success",
    calls: 150,
    index_artifact: {
      key: "d".repeat(64),
      artifact_id: 10,
      run_id: 100,
      builder_commit: requestFixture.baseline_sha,
    },
    historical_index: {
      performance: summarizePerformance([
        {
          id: "full-corpus",
          index: { ...stage(), index_bytes: 4096 },
          query: {},
        },
      ]),
    },
    performance: summarizePerformance([
      {
        id: "full-corpus",
        index: null,
        query: Object.fromEntries(
          ["hybrid", "fts", "vector"].map((mode) => {
            const samples = Array.from(
              { length: 50 },
              (_, i) => (i + 1) * scale,
            );
            return [
              mode,
              {
                ...stage(),
                failed_calls: 0,
                samples_ms: samples,
                latency_ms: statistics(samples),
              },
            ];
          }),
        ),
      },
    ]),
    quality: ["hybrid", "fts", "vector"].map((mode) => ({
      mode,
      completed: 10,
      planned: 10,
      metrics: {
        file_hit_at_1: 0.5,
        file_hit_at_5: 0.8,
        file_hit_at_10: 1,
        file_mrr_at_10: 0.7,
        ndcg_at_10: 0.6,
      },
    })),
  });
  return {
    schema_version: 1,
    kind: "browsecomp-doc-comparison",
    status: "success",
    harness_commit: requestFixture.harness_commit,
    document_count: 100195,
    model_sha256:
      "217e3074d135c2055eed9d3b3c43a3158a904460e4e6ca4b3d492a7ca4f93771",
    repository_url: "https://github.com/owner/repo",
    run_url: "https://github.com/owner/repo/actions/runs/123",
    baseline: arm(requestFixture.baseline_sha, 2),
    candidate: arm(requestFixture.candidate_sha, 1),
  };
}
