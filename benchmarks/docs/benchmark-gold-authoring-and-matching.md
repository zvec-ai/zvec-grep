# Retrieval-only QA pair authoring and matching

Each case contains its original question and the files (File) and source targets, such as functions, classes, or evidence passages (Entity), that retrieval should find. These expected targets are the **Gold**.

Gold must include every expected target within the specified source version and search scope.

## Authoring Gold

1. Preserve the original question. Specify the source version and searchable files.
2. Annotate File and Entity targets separately, with source locations and relevance explanations.
3. Have an independent reviewer check that targets are correct and none are missing. Record the conclusions.
4. Cases that have not passed correctness and completeness review must not enter formal evaluation.
5. Resolve the question's meaning and scoring rules before evaluation.
6. Changes to parser IDs or chunk boundaries do not change the Gold.
7. Preserve original document and passage judgments. For Entity Gold, identify the specific evidence in the pinned source. Record a reason when the judgment cannot be narrowed to a verified span.

## Matching results

- Check retrieved results against the specified source version.
- File matches require the full repository-relative path.
- Match each Gold source span by full containment in one returned Entity's `content` in the same file. Partial overlap is a miss.
- One result may cover several Gold targets. Count each target once, at the rank where it first becomes fully covered.
- Preserve alternative evidence groups from the source dataset. Cover every span in any one group to hit the target; other alternatives do not add credit.
- `preview` affects display and information density, not Entity matches or Recall.
- Take the first K results in their original order. Duplicate results still occupy their original positions.
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

## Aggregation and comparison

- For Hit, Recall, RR, and nDCG, average repeated runs within each case first, then average across cases with equal case weights.
- Result tables show scores only. Keep scored/eligible counts and unscored-case reasons in machine-readable reports, without repeating counts beside the tables.
- Use the same questions, source and search scope, Gold, scoring rules, and return limits on both sides of a comparison.

Follow the [benchmark working rules](../AGENTS.md) for evidence storage, version changes, and Git submissions.

Use the [field and scoring reference](retrieval-only-qa-reference.md) when preparing data or implementing scoring.
