# AACR code-review searches

Six selected searches from OCR reviews of three public PRs. This is a small
retrieval-only development suite, not a random sample or an OCR quality test.
There is no reviewing agent, query rewrite, answer model or Judge.

| ID | PR | Query |
| --- | --- | --- |
| Q02 | [Union #3414](https://github.com/unionlabs/union/pull/3414) | finality_update finalized_header execution timestamp |
| Q03 | [Filament #15217](https://github.com/filamentphp/filament/pull/15217) | TextColumn time method formatting |
| Q04 | Union #3414 | block_timestamp microseconds Aptos |
| Q05 | Union #3414 | EthereumConsensusState timestamp field |
| Q07 | [Timescale #8138](https://github.com/timescale/timescaledb/pull/8138) | compressed table schema creation column order |
| Q09 | Union #3414 | ExecutionPayload timestamp |

`data/aacr.json` pins queries, commits, Git trees and index selection. Q07 is
the first phrase decoded from a JSON-shaped natural tool input; the original
is preserved in the source-review archive. Keep queries from the same PR in the
same development/test partition.

## Gold and scope

Q02–Q05 and Q07 have twelve File and seventeen Entity targets. Q09 is an
exact-lookup control with one File and one Entity target, reported separately
and excluded from semantic means. Operational counts include all six queries.

Gold uses the [shared authoring and matching rules](../../docs/benchmark-gold-authoring-and-matching.md):
source byte ranges, full containment and any complete accepted evidence set.
Scores average five calls per query before averaging queries. Native ranks and
duplicates remain unchanged. A File hit does not establish complete evidence.
Stage diagnostics include all eighteen Entity targets; use task IDs to inspect
semantic coverage alone. Known retrieval misses remain diagnostic results.

`gold/aacr-review.json` records independent source-review scope and limitations.
Labels were reviewed by AI (exact model unknown), not a human; the sealed
process does not guarantee perfect blindness or exhaustive answers. Q02 covers
the local timestamp read/type chain, Q04 the local Aptos conversion/import, and
Q05 one Rust type/field declaration. External time-unit specifications and
conditional ABI/protobuf representations are not mandatory answers. Q07 covers
new physical compressed chunks, not every historical or ALTER layout.

The contributor retains the sealed review archive identified by filename and
SHA-256 in that summary; obtain it to audit original findings and adjudications.
Historical questions, Gold revisions and raw traces stay in that archive rather
than becoming runtime dependencies. Public pinned sources suffice to reproduce
retrieval. Source excerpts retain upstream licenses and notices in
`gold/aacr-source-licenses.json`; Timescale includes its TSL terms.

## Reproduction

Use the [shared setup and native-package instructions](../README.md#run-swe-qa20-locally)
and [native build prerequisites](../../../rust/docs/development/building.md).
From the repository root, with a built native tarball:

```sh
node --test benchmarks/zg-retrieval/test/*.test.mjs \
  .github/scripts/retrieval-pr-comment.test.mjs

RETRIEVAL_EMBEDDING=local node benchmarks/zg-retrieval/expansion/run.mjs \
  --suite aacr \
  --package /path/to/native-package.tgz \
  --output /path/to/new-results \
  --model-cache /path/to/model-cache \
  --candidate-commit "$(git rev-parse HEAD)" \
  --gold-stages
```

The output directory must not exist. Preparation installs the package, fetches
three pinned repositories and builds fresh indexes. It can download the CPU
`local/potion-code-16m-v2` weights; no cloud embedding API is required. Record
actual package, model and harness hashes from the output when sharing a run.
Gold and review records remain outside searchable repositories.

The shared protocol uses Hybrid, FTS and Vector, five calls each: **90 searches**
(75 semantic, 15 control), Top 10 and full preview. Native ignore rules, frozen
code extensions and the 1,000,000-byte limit remain. Index timeout is 60 minutes
per repository; search timeout is 120 seconds per call. Missing calls and failed
source/model/index identity checks are operational failures, not quality zeros.

CI runs this suite through the existing retrieval workflow, report aggregation
and PR publication. Reports and raw evidence artifacts are retained for 14 days;
archive them separately for longer retention. Never relabel historical runs
with a new input or Gold identity.
