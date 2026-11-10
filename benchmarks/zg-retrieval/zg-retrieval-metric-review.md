# Retrieval-only metric rationale and limitations

The benchmark measures whether ZG retrieves and ranks annotated relevant files for 20 frozen SWE-QA questions across 11 pinned repositories. All three modes use the same 39 file targets and the Rust MCP default presentation. See the [test design](./zg-retrieval-only-sweqa20-design.md) for execution, formulas and artifact contracts.

## What the quality metrics measure

A result is relevant when its public file path matches a labeled file in [`gold/files-v1.json`](./gold/files-v1.json). Matching normalizes path separators and accepts exact paths or directory-boundary suffixes, without case folding. It does not inspect declarations, outlines, source text or snippet boundaries.

The labels contain one target per unique accepted file path for each question. Bridge-only files do not earn credit. A fragment from any location in a labeled file can count as a hit; the score does not establish that the returned content answers the question.

| Metric | Purpose | Important limit |
| --- | --- | --- |
| File Hit@1 | Measures whether the first result locates a labeled file | One fragment from any labeled file is sufficient |
| File Hit@5 / File Hit@10 | Measures whether a labeled file is found within the inspection budget | Finding more labeled files does not increase a question's binary Hit score |
| File MRR@10 | Rewards locating the first labeled file earlier | Later relevant results do not contribute |
| nDCG@10 | Rewards early retrieval of distinct labeled targets relative to their ideal ordering | Depends on the completeness and correctness of the target set |

Native ranks are preserved. Several chunks from the same file occupy several result positions; they are never collapsed or renumbered. Each nDCG target contributes only at its first matching rank, with binary gain at each rank. Unretrieved targets remain in the ideal-gain denominator. This penalizes duplicate-file results that consume positions without locating further labeled files.

Each question/mode first averages its five repeated call-level scores. Hit and MRR then average the 20 questions equally. nDCG averages within each repository, then gives each of the 11 repositories equal weight. These metrics can move differently because they reward different ranking behavior and use different aggregation weights.

## Why file relevance is used

File matching evaluates location and ranking independently of how the product formats retrieved content. With the same ranked file paths, changing preview length, adding an outline or displaying a declaration cannot change a quality score. This makes the metrics useful for diagnosing file retrieval regressions without making output formatting a relevance requirement.

The metrics support concrete investigations: a Hit@10 regression means a labeled file disappeared from the search budget; stable Hit@10 with lower MRR suggests its first result moved later; lower nDCG can indicate that fewer labeled files were found or that they appeared later. Raw per-question results are needed to identify which case occurred. A miss alone cannot identify whether candidate generation, fusion, aggregation or ranking caused it.

## Label limitations

The 39 targets are projected from accepted paths in AI-assisted source annotations. They are partial positives and have not received independent blind human validation. Source anchors remain annotation provenance; they do not participate in scoring. The accepted/bridge distinction can exclude useful supporting files.

File scoring reduces sensitivity to presentation, but it cannot remove bias in the choice of labeled files. It does not measure complete recall, evidence sufficiency or answer correctness. Relevant unlabeled files receive no credit, and an unhelpful fragment from a labeled file can receive credit. Therefore, the current scores are evidence about annotated-file localization rather than comprehensive retrieval quality.

The suite is small and public. A question that changes consistently across all five calls changes a mode's Hit score by five percentage points; a change in one of its five calls changes it by one point. Five repeated calls remain one question/mode vote, not five independent questions. The report separately identifies unstable ordered Top-10 results, since equal quality scores need not imply identical retrieval. Use the suite for development regressions and paired per-question diagnosis; it does not by itself justify broad claims about retrieval quality.

A stronger evidence benchmark would require separately reviewed logical source spans and acceptable alternatives. Candidate evidence should be pooled from varied retrieval methods and manual inspection, then reviewed without revealing its origin. Annotation agreement and task outcome validation would help establish whether those labels measure useful answer evidence.

## Operational measurements and interpretation

Mean output size measures the UTF-8 bytes of successful fifth-call public MCP text, expressed in KiB. It is not a token count or a quality score. Avg RT and P50 RT use all successful calls, excluding indexing. Fixed hybrid → FTS → vector order and shared runtime/model caches limit their use as a controlled speed comparison.

Installation, indexing and product-call failures fail operational integrity. Missing quality results from product failures retain zero credit in the denominator; failed calls do not enter output-size or latency aggregates. Invalid experiments withhold aggregate scores. No arbitrary quality threshold determines CI success.

## Scoring and compatibility

The file-ranking contract is `sweqa-file-hit-rr-v1`. The nDCG implementation is checked against the byte-identical pinned Python scoring reference in [`test/fixtures/ndcg-reference/`](./test/fixtures/ndcg-reference/), with its MIT attribution preserved. This reference is used only in unit tests.

Protocol `sweqa20-zg-rust-three-modes-mcp-default-v7` fixes the original questions, repository commits, file labels, Rust MCP default presentation and three retrieval modes. ZG evidence uses schema 7: `quality_mean` contains five-call Hit/MRR/nDCG means and ranking stability; the representative fifth-call `file_retrieval` and `ndcg` retain target evidence. The CI overview uses schema 5 with fixed `zg-hybrid`, `zg-fts` and `zg-vector` rows.

Comparisons require matching protocol and frozen inputs. Public responses are reparsed for scoring, and validators recompute aggregates instead of trusting saved derived values. Reports from other protocol versions require their matching scorer checkout; replaying saved evidence is not a new retrieval run.
