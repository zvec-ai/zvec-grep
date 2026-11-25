# Rule ranking

The shared indexed-search engine uses RRF fusion to assign result positions for
direct, server, and MCP queries. With `--prefer-symbol`, hits from an exact-symbol
recall route win RRF ties, then entity ID breaks any remaining ties.
Non-code entities keep their fusion order and raw RRF scores. They keep their
positions while distinct code candidates are available. Positions occupied by
code within the result limit are filled by ranking all recalled, loaded `ContentKind::Code`
entities, including those below the cutoff and those without symbol metadata.
The engine skips rule ranking when the visible results contain no code.

The policy normalizes RRF by the maximum possible score for the enabled routes,
adds bounded symbol/definition bonuses, and applies the minimum applicable path
factor. Name evidence comes from entity metadata, not arbitrary mentions in
source text. Unknown metadata is neutral. Constants and aliases are not
penalized for being short.

Positive path affinity comes from the file stem and the two nearest directories,
with a maximum bonus of 0.35. Each query term contributes once, with half credit
for directory matches. Explicit file paths receive full affinity. Weak name-token
coverage and path affinity share a lexical budget rather than adding both in full.
Name-token bonuses also depend on distinct query-token coverage; incidental
Value matches receive one quarter of the weak bonus, except explicit field or
constant queries. Exact names retain their full bonus.

Definition bonuses require an exact or qualified name match and an eligible
symbol kind. High-confidence bare symbols receive 0.85; a single explicit
identifier embedded in a longer query receives 0.40; multiple identifiers
receive 0.10. Explicit symbol-kind requests must match the candidate kind.
Usage queries disable definition bonuses. Queries asking for implementations
do not boost the interface/trait declaration itself. Static name/definition/path
bonuses are capped at 1.25 in the normalized RRF scale.

Path roles use workspace-relative components and explicit filename patterns.
Test/example/dependency intent and narrowly targeted include globs can waive a
role penalty. A common dependency penalty is neutralized when all code candidates
are dependency code. Neither `dist/` nor `.d.ts` alone implies generated code.

Final selection is greedy: after each selected entity, remaining entities from
the same file receive a soft factor of `1 / (1 + 0.5 * selected_count)`. There is
no hard per-file cap. A code pool from a single file skips this selection
penalty.

If a class and its method both earn a result slot, the enclosing symbol covers
both source spans. A query for a specific symbol keeps the direct match. The
selector fills the freed slots from the remaining pool and skips symbols whose
source contains or is contained by an existing selection in the same file.
Sibling methods can still appear together. Once distinct code hits run out,
subsequent documents can move up, and the result can be shorter than the limit.
Across query groups, group coverage and global rank determine which of two
nested group results appears in the merged list.

Before selection, a file's representative definition can gain up to 0.15 from
at most two additional definitions. Support must have name or path evidence,
distinct scoped names and non-overlapping source ranges. Values and unknown
ranges cannot supply support. The support term uses bounded base scores and
the same path prior; it does not sum every fragment in a large file. It is an
extra term after the static bonus cap.

For code hits, `score` is the rule selection score. In mixed results, use
`rank` for order because code and non-code scores use different scales.
Trace preserves the original `fusion` rank and score for all hits. For code hits,
`ranking` records the policy identifier, query features, name match/bonus,
definition bonus, path roles/exemptions, static score, positive `path_bonus`,
`file_support_bonus`, `file_support_count`, selected file count, diversity factor,
and group-local final rank.
`path_overrides` lists roles exempted by query/scope, including those not present
on a particular hit. Without trace, detailed explanation strings are not built.
Serialized traces may omit `ranking`. Within a ranking trace, omitted
`path_bonus`, `file_support_bonus`, and `file_support_count` fields default to zero.

The loader batches the whole candidate pool, including entity content, before
ranking and selection. Monitor both `load_results` and `ranking` timings to
account for candidate loading and scoring costs. Ranking uses stored metadata
without scanning source files or widening recall.

The tests exercise definition promotion before the result limit, symbol roles,
path exceptions, deterministic ties, and dynamic file diversity. Validate
weight changes by language and query intent; file-only labels can overstate
the benefit of spreading results across files.

An opt-in ignored test replays local code candidate snapshots through this same
scorer. Set `ZG_RANK_REPLAY` to the snapshot directory and explicitly run
`replay_rule_ablation` with `--ignored`. The variable is read only by the test;
production direct/server behavior cannot be changed through it. Internal policy
variants provide baseline, single-feature, and leave-one-out comparisons without
creating a separate ranking implementation.
