# Retrieval-only QA pair authoring and matching

Each case contains its original question and the files (File) and code targets, such as functions or classes (Entity), that retrieval should find. These expected targets are the **Gold**.

Gold must include every expected target within the specified source version and search scope.

## Authoring Gold

1. Preserve the original question. Specify the source version and searchable files.
2. Annotate File and Entity targets separately, with source locations and relevance explanations.
3. Have an independent reviewer check that targets are correct and none are missing. Record the conclusions.
4. Cases that have not passed correctness and completeness review must not enter formal evaluation.
5. Resolve the question's meaning and scoring rules before evaluation.
6. Changes to parser IDs or chunk boundaries do not change the Gold.

## Matching results

- Use only information the tool actually returned, checked against the specified source version.
- File matches require the full repository-relative path.
- Entity matches require the same code target. Returning a class does not automatically match its methods.
- Each result can receive credit for at most one Entity.
- Take the first K results in their original order. Repeated targets count once but still occupy their original positions.
- If a score cannot be determined, record why instead of assigning zero. Assign zero only to a confirmed miss in valid results.

## Retrieval-only metrics

K is the number of original result positions examined. Compute each metric separately for File and Entity.

| Metric | Meaning or calculation |
| --- | --- |
| Hit@K | 1 if the first K results contain at least one Gold target; otherwise 0. |
| Recall@K | Distinct Gold targets found in the first K results divided by the number of distinct Gold targets. |
| First-hit rank | The first Gold target's original position; record "not found in top K" for a miss. |
| RR@K / MRR@K | RR is the reciprocal of the first-hit rank, or 0 for a top-K miss. MRR is the mean RR. |
| nDCG@K | Measures whether relevant targets appear early; see the [calculation](retrieval-only-qa-reference.md#ndcgk-calculation). |

Evaluate target matches and the completeness of returned content separately.

## Aggregation and comparison

- For Hit, Recall, RR, and nDCG, average repeated runs within each case first, then average across cases with equal case weights.
- Report the number of scored cases and list unscored cases with reasons.
- Use the same questions, source and search scope, Gold, scoring rules, and return limits on both sides of a comparison.

Follow the [benchmark working rules](../AGENTS.md) for evidence storage, version changes, and Git submissions.

Use the [field and scoring reference](retrieval-only-qa-reference.md) when preparing data or implementing scoring.
