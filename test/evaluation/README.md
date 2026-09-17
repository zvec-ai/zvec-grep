# Default CLI evaluation

Run from the repository root after building:

```sh
ZVEC_GREP_MODEL_CACHE=/path/to/prepared/local/cache node test/evaluation/default-search.mjs
```

This opt-in evaluation runs the actual CLI with only a positional query. It copies
the real `src` tree into isolated temporary workspaces, excluding tests, labels,
reports, documentation, and dependency trees. It uses the default local Potion
model and an isolated home/loopback daemon, never a remote embedding service.

Each cold sample is the first query against a new workspace without an index but
with an already downloaded model. Warm samples share an implicitly prepared
index. NDJSON stdout records the revision, preparation time, query latency,
returned locations, compiled-build/corpus/label checksums, full stdout/stderr, and each verdict. Exact symbols and paths
require top 1; phrases and questions require relevant evidence in the first 5
displayed results. The returned source range must cover the labeled line, not merely name
the right file. A broad container range can pass this localization check even
if the preview is unhelpful; snippet quality needs separate inspection.
Negative queries require no matches. Nonzero exit indicates a
harness/runtime error; quality failures are reported without aborting the run.

The 26 questions are a **curated development set**, not a held-out benchmark or
a sample of user traffic. The success fraction does not establish that defaults
work for 70% of users. Uncached model downloads, stale/offline workspaces, other
repositories/languages, terminal visual quality and resource usage still need
separate measurements. Keep labels outside the indexed corpus. Update a label
only when source movement changes its location or independently reviewed intent
changes, not to make a failed retrieval pass.

For controlled before/after runs, first copy `src` outside the repository and set
`ZVEC_GREP_EVAL_SOURCE` to that frozen source tree. Both runs should have the same
corpus and label checksums. The running CLI still comes from the current build.

For another repository, stage a source-only snapshot (no labels, reports or
dependency trees) and set `ZVEC_GREP_EVAL_SOURCE` to it. Set
`ZVEC_GREP_EVAL_CASES` to a separate JSON object with `label` (include its pinned
revision), `layout: "root"` to preserve repository-relative paths, and `cases`
using the same fields as `default-cases.mjs`. Labels are validated before search,
and source-line needles must identify exactly one line. The default corpus and
cases are unchanged when this variable is absent. Freeze new questions before
their first run; curated cross-repository cases are still not user traffic.

Set `ZVEC_GREP_EVAL_DIAGNOSTICS=1` to additionally run the semantic/mixed questions
with FTS-only and vector-only routes. These diagnostic variants are explicitly
labeled and excluded from the default success/latency summaries.

For local model comparisons, prepare the candidate separately, then set
`ZVEC_GREP_EVAL_MODEL=local/<catalog-model>` for the evaluation run. This changes
only the isolated test environment, not the shipped default or user config.
The report marks the override. No remote model is accepted.

```sh
ZVEC_GREP_MODEL_CACHE=/path/to/cache node test/evaluation/prepare-model.mjs local/potion-multilingual-128m
```

Preparation reports download/loading time separately from one warm query and
the preparation process's peak RSS. The indexing wait allows up to ten minutes
for slower comparison models; its measured duration is not hidden from results.
Index preparation also records file/entity counts and truncation diagnostics.
Models can have different input-token budgets, which change source chunking;
these runs compare end-to-end configurations, not embeddings in isolation.
