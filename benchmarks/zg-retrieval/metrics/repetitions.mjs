import assert from "node:assert/strict";
import { scoreFileRetrieval } from "./files.mjs";
import { scoreNdcg } from "./ndcg.mjs";

const FILE_FIELDS = ["hit_at_1", "hit_at_5", "hit_at_10", "rr_at_10"];
const mean = (values) =>
  values.reduce((sum, value) => sum + value, 0) / values.length;

/** Keep only public ranking fields needed for file scoring and stability. */
export function rankedLocations(items) {
  return items.map(({ rank, path, range, matched_by }) => ({
    rank,
    path,
    range,
    matched_by,
  }));
}

/** Compare the ordered public locations, not just the resulting metric values. */
export function rankingSignature(items) {
  return JSON.stringify(
    rankedLocations(items).map(({ rank, path, range, matched_by }) => [
      rank,
      path,
      range ?? null,
      matched_by ?? null,
    ]),
  );
}

/** One query/mode has one vote, obtained from its five repeated calls. */
export function summarizeRepeatedQuality(
  calls,
  targets,
  { includeFileRecall = false } = {},
) {
  assert.equal(calls.length, 5, "quality requires five calls per query/mode");
  assert.deepEqual(
    calls.map((call) => call.repetition),
    [1, 2, 3, 4, 5],
    "quality requires ordered repetitions",
  );
  const scores = calls.map((call) => {
    assert.ok(Array.isArray(call.items), "missing public ranked items");
    return {
      file: scoreFileRetrieval(call.items, targets),
      ndcg: scoreNdcg(call.items, targets),
    };
  });
  const rankings = new Set(calls.map((call) => rankingSignature(call.items)));
  return {
    file: {
      ...Object.fromEntries(
        FILE_FIELDS.map((field) => [
          field,
          mean(scores.map((score) => score.file[field])),
        ]),
      ),
      ...(includeFileRecall
        ? Object.fromEntries(
            [1, 5, 10].map((k) => [
              `recall_at_${k}`,
              mean(
                scores.map(
                  ({ file }) =>
                    file.target_ranks.filter(
                      (rank) => rank !== null && rank <= k,
                    ).length / file.n_relevant,
                ),
              ),
            ]),
          )
        : {}),
    },
    ndcg_at_10: mean(scores.map((score) => score.ndcg.ndcg_at_10)),
    ranking_repeatable: rankings.size === 1,
    unique_rankings: rankings.size,
    hit_at_10_calls: scores.reduce(
      (sum, score) => sum + score.file.hit_at_10,
      0,
    ),
  };
}
