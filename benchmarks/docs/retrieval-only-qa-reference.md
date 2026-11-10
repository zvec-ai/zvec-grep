# Retrieval-only QA pairs: fields and scoring details

Use this reference when preparing data or writing scoring code. For the core rules, see the [authoring and matching guidelines](benchmark-gold-authoring-and-matching.md).

## Authoring and review

- The annotator and reviewer each read the question and source code first, then use reference answers to check for gaps.
- Review must check both whether the targets are correct and whether any are missing. Record the conclusions.
- Questions must pass both correctness and completeness review before entering formal evaluation.
- Do not inspect the evaluated tool's name, retrieval ranks, or scores while authoring or reviewing.
- Reference answers, Gold, and reports must not be searchable. The original source code that Gold points to remains searchable.
- Record the File containing each Entity. Verify File and Entity targets separately.
- Do not list the same Gold target more than once because it has aliases or appears in multiple code fragments.
- When both a class and one of its methods are in Gold, explain which fact each supports.
- When several code locations must be used together, or can substitute for one another, record how they support the specific fact.
- Record why candidates were excluded or still need verification. Do not include them in Gold.
- For questions with multiple interpretations, keep evidence for each interpretation. Exclude the question from aggregate results until the scoring rules are settled.
- Save rewritten questions as a separate version.

## Reading and matching results

### File

- Agree on path separators and the project root before evaluation.
- Convert paths to repository-relative paths, preserving case.
- When removing the root prefix, match complete directory names rather than just the first few characters.
- A matching filename or path suffix alone does not count as a hit.
- Paths outside the project directory are invalid.
- If the root is unclear or several files could match, record the match as uncertain.

### Entity

- Internal IDs, names, and aliases help locate a target but do not prove a hit on their own.
- Overlapping line ranges, or a returned class containing the target method, do not automatically count as a hit.
- Do not split multiple code targets within one result into separate scored items.
- Source code read during verification must not be treated as content returned by the tool.
- Do not regenerate Gold using the parser under evaluation.
- Do not give credit for information obtained from an internal index but absent from the tool's response.

### Order and scoring eligibility

- Specify which result group to use and how to read its order in advance. Do not adjust these choices based on Gold.
- Duplicate results keep their original positions. Do not remove them and pull later results into the top K.
- Before scoring at either level, its Gold must pass correctness and completeness review.
- The first K results must be complete, or there must be evidence that the tool returned all available results.
- If uncertainty could change a metric, leave that metric unscored rather than assigning zero.
- Record failures, missing data, parsing errors, limit violations, and valid misses separately.
- Keep the first-hit rank for each run. Do not use 0 or K+1 for a miss.

## nDCG@K calculation

1. Each distinct Gold target contributes `1 / log2(rank + 1)` only at its first hit.
2. Sum the contributions within the first K results to obtain the actual score.
3. Place `min(K, number of Gold targets)` correct targets at the top of the ranking and apply the same formula to obtain the ideal score.
4. Divide the actual score by the ideal score to obtain nDCG@K.

## Reporting and version changes

Include the following in reports:

- The number of scored questions and calls for each metric, and the proportion of questions with usable Gold.
- Reasons for leaving results unscored, plus counts of failures and missing data.
- The questions that can be scored for both versions being compared.
- Latency for the first call and subsequent calls separately.
- Whether repeated runs of the same question produce the same ranking and returned content.

Report rescoring saved responses separately from scoring new retrieval runs.

Preserve historical File metrics, the old rules that match code start and end positions, and their failure handling in their original versions.

When Gold or scoring rules change, recalculate both sides using the same new rules. List any information missing from historical records.

When source targets move, are renamed, split, merged, or deleted, review them again and publish a new Gold version.

When missing correct targets are found, review them independently before publishing a new Gold version.

The suite README must list the metrics and K values actually supported, and the scoring code version.

When writing or changing scoring code, check cases involving changed code boundaries, a returned whole class, duplicate results, and uncertain target matches.

## Fields to save

Checksums help detect changes to saved content.

| Category | Contents |
| --- | --- |
| Question | Original text, question set version, and text checksum. |
| Source code | Full commit ID, source archive checksum, allowed search files, and filtering conditions. |
| File Gold | Repository-relative path, evidence location, relevance rationale, and checksums for the file and evidence content. |
| Entity Gold | Containing file, type, full name, source range, key code, and its checksum. |
| Non-unique names | Containing class, enclosing function, or other information that distinguishes targets with the same name. |
| Review process | Annotator and reviewer, initial drafts, reference material already seen, disagreements, and final conclusions. |
| Verification status | Separate File and Entity statuses: verified, needs evidence, disputed, or not applicable. |
| Run configuration | Invocation entry point, model and retrieval settings, output format, K values, and result count and length limits. |
| Returned content | Raw responses, whether they contain previews only, and how overlong content is truncated. |

Use these position and checksum conventions:

- Line numbers start at 1 and include both endpoints.
- Byte offsets start at 0; the end offset is excluded.
- Encode the original question as UTF-8 and calculate its checksum with SHA-256.
- Calculate file and source fragment checksums from the original bytes, without first changing whitespace or line endings.
