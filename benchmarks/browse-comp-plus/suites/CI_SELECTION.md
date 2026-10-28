# Fixed five-case CI selection

The daily `ci` suite uses the original **628, 700, 966, 827, 241** cases.
Each case runs one baseline/zg pair: **10 answers and 10 judgements**.
Five concurrent case processes share one restored corpus/index and daemon.
Keep `iteration` (628/241) for infrastructure changes.

The user requested restoring these five cases after the ten-case experiment was
too slow. This changes the suite size; it does not replace individual cases by
observed score or speed. Case 827 remains included despite its long failed trial
in the ten-case run. Earlier ten-case evidence and costs remain historical data.

## Selection basis

Restore the original five-case sample without changing its seed or order.
For all 830 query IDs at dataset revision
`144cff8e35b5eaef7e526346aa60774a9deb941f`, compute
`SHA256(UTF8(revision + NUL + "browsecomp-plus-ci-v1" + NUL + query_id))`,
sort the hexadecimal digests ascending, and retain the first five. There are no
manual substitutions. Query wording
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
answers are reproduced here. The annotated evidence sets cover 32 distinct files.

| Case | Gold / evidence files | Retrieval structure covered |
| --- | ---: | --- |
| 628 | 5 / 6 | Link a film, director biography, editing credits, and production-design roles across works; disambiguate people by their roles. |
| 700 | 4 / 7 | Connect developer histories and corporate timelines with game credits and indirectly described characters. |
| 966 | 6 / 7 | Reconcile building identity across several historical periods, people, later reconstruction, and current site descriptions. |
| 827 | 1 / 6 | Follow subject → descendant → book author → organization, then separate a namesake using cemetery evidence. |
| 241 | 6 / 6 | Combine biography, migration/marriage, family history, and a letter with indirect descriptions of a historical figure. |

These cases cover film/game/history, genealogy, biography, and indirect entity
links. This fixed sample does not claim exhaustive topic coverage or certify
every source clue independently.

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
by twenty percentage points; five cases can expose large regressions but cannot
establish small general improvements. Use a larger study for release claims.

If a dataset defect is later confirmed, record the evidence and version the
suite rather than silently replacing a losing or slow case. A changed suite
creates a new plan; never resume answers from a different suite plan into it.
