# SWE-QA20 QA pairs

`qa-pairs-v1.json` keeps the 20 original questions and pinned repository commits
from `data/source.lock.json`. It adds 65 reviewed file targets and 114 reviewed
entity targets from the historical S2 review.

## Review status

| Use | Cases | Meaning |
| --- | ---: | --- |
| Diagnostic scoring | 17 | Known positive targets recorded in the historical S2 annotation bundle. |
| Excluded from diagnostic scoring | 3 | `streamlink:14`, `django:32`, and `streamlink:43` have unresolved question interpretations. |
| Formal scoring | 0 | This frozen version is published for diagnostic use. |

`diagnostic_eligible` permits a diagnostic score only. It does not override
`scoring_eligible`, which remains false for every case. Do not present a score
against these labels as formal Entity Recall or as complete answer coverage.
Recall uses the recorded targets as its denominator. The three disputed cases
retain their alternative interpretations and targets; they are not silently
dropped or assigned zero. Historical review fields remain in the frozen export
for reproducibility; they do not prove full-corpus coverage.

The existing `files-v1.json` and its historical File scores are unchanged. The
reviewed QA targets have a different denominator and must be reported separately.

## Contents

Each case records its original query, source commit, review state, file targets
and entity targets. Targets include source paths, byte and line ranges,
checksums, review decisions, and reasons for relevance. Entity declarations and
definition text come from the pinned source archive, not the tested parser.
The target IDs identify annotations, not ZG index entities.

Entity scoring checks whether retrieved `content` contains each Gold target's
full range in the pinned source file. A result may cover several targets; each
target counts once at its first native rank. Preview length does not affect
matching. See the [matching rules](../../docs/benchmark-gold-authoring-and-matching.md).

`provenance` records the hashes of the original review files. `source_licenses`
preserves the source repositories' license files with commit-pinned links.
Embedded source excerpts retain their original bytes and licenses. Gold and
source excerpts must stay outside the indexed corpus.

## Re-export and validate

Re-exporting requires the original review bundle and its pinned source archives:

```sh
python3 benchmarks/zg-retrieval/gold/import-qa-pairs.py \
  --release "$qa_release" \
  --archive-root "$qa_archives"
npx prettier --write benchmarks/zg-retrieval/gold/qa-pairs-v1.json
node --test benchmarks/zg-retrieval/test/qa-pairs.test.mjs
```

The archive directory contains `<full-commit>.tar.gz` files. Without
`--archive-root`, the importer reads archive locations from the supplied review
bundle's input lock. These machine paths never enter the exported dataset.

The importer verifies archive commits and checksums, exact source spans, and
Python/C declaration identity. These checks establish source consistency.
For a new Gold version, reconcile all known targets from the declared annotation
sources, record omissions and unresolved items, and rescore both sides using
that version. Exhaustive whole-corpus judgment is not required.
