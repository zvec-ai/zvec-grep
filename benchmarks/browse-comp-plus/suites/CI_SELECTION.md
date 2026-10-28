# Fixed ten-case CI selection

The daily `ci` suite uses **628, 700, 966, 827, 241, 165, 1068, 244, 843, 563**.
Each case runs one baseline/zg pair: **20 answers and 20 judgements**. Keep
`iteration` (628/241) for infrastructure changes. Five concurrent case processes
share one restored corpus/index and daemon; concurrency does not limit the suite
to five cases. The previous five-case CI results remain historical measurements
of that earlier suite, not completed validation of this ten-case definition.

## Selection basis

Extend the original sample from five to ten without changing its seed or order.
For all 830 query IDs at dataset revision
`144cff8e35b5eaef7e526346aa60774a9deb941f`, compute
`SHA256(UTF8(revision + NUL + "browsecomp-plus-ci-v1" + NUL + query_id))`,
sort the hexadecimal digests ascending, and retain the first ten. There are no
manual substitutions. The original five remain the first five. Query wording
and annotated document IDs describe coverage; answer correctness, zg adoption,
tokens, and runtime do not decide inclusion. This avoids selecting known zg wins
or dropping expensive/unsuccessful cases to improve reported results.

The selected cases span multiple files and subject areas, indirect entity links,
chronology, namesake disambiguation, and citation matching. These are **document
retrieval tasks, not code-repository tasks**. Cross-subject evidence chains do not
measure cross-module code navigation or call-graph understanding. That capability
needs a separate repository benchmark.

## Per-case coverage

Counts below are distinct IDs in the pinned dataset's `gold_docs` and
`evidence_docs`. They are **annotations, not a minimum number of files an agent
must read, proof of an indispensable multi-hop path, or a difficulty score**.
Coverage descriptions come from the task wording; no full questions or reference
answers are reproduced here. The annotated evidence sets cover 56 distinct files.

| Case | Gold / evidence files | Retrieval structure covered |
| --- | ---: | --- |
| 628 | 5 / 6 | Link a film, director biography, editing credits, and production-design roles across works; disambiguate people by their roles. |
| 700 | 4 / 7 | Connect developer histories and corporate timelines with game credits and indirectly described characters. |
| 966 | 6 / 7 | Reconcile building identity across several historical periods, people, later reconstruction, and current site descriptions. |
| 827 | 1 / 6 | Follow subject → descendant → book author → organization, then separate a namesake using cemetery evidence. |
| 241 | 6 / 6 | Combine biography, migration/marriage, family history, and a letter with indirect descriptions of a historical figure. |
| 165 | 4 / 6 | Bridge statistical reporting → country → film → director biography, checking date and age constraints. |
| 1068 | 5 / 5 | Match an author's life in several places, a business, a literary event, and bibliography without relying on a supplied name. |
| 244 | 2 / 2 | Locate two publications and align their shared citation despite different reference formats and positions. Two files still require a structural join. |
| 843 | 1 / 7 | Follow a thesis and advisor biography to a game/DLC, distinguish content changes, and locate an ordered credit entry. |
| 563 | 3 / 4 | Join education, executive history, acquisition timing, and interview clues that refer indirectly to other people. |

The five additions broaden the existing film/game/history coverage with a
statistics-to-film bridge, literature, scientific references, academia-to-game
credits, and industry biography. This review describes the fixed sample; it does
not claim exhaustive topic coverage or certify every source clue independently.

## Reproducibility and interpretation

[`ci-selection.json`](./ci-selection.json) records the pinned revisions, full
decoded-query SHA256, rank hashes, document IDs/counts, and coverage tags. Counts
and ranks were checked against all 830 decoded queries, whose bytes match the
verified CI preparation manifest. No index rebuild or model calls are needed to
select the suite. The audit file is documentation, not an agent prompt or grader
input; questions, reference answers, and scoring stay unchanged.

Keep this suite fixed for daily regression comparisons under the same model and
protocol. Report accuracy, mean and median tokens/time, per-case paired changes,
zg adoption/tool errors, and failed/unknown costs. A single case changes accuracy
by ten percentage points; ten cases can expose large regressions but cannot
establish small general improvements. Use a larger study for release claims.

If a dataset defect is later confirmed, record the evidence and version the
suite rather than silently replacing a losing or slow case. A changed suite
creates a new plan; never resume answers from the earlier five-case plan into it.
