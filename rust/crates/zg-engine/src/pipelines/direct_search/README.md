# Direct search

Direct search serves `--rg` and the literal-search fallback after filename lookup.
Ordinary text files are searched directly with the embedded grep matcher; no
workspace index is required. PDF files use canonical text from an existing
extraction index, never their raw binary bytes.

The coordinator owns requested scope, result ordering, and final limits. The
extracted reader returns hits and files without a usable index. Directory and
workspace searches skip those files and report one aggregate indexing hint;
an explicitly requested unindexed file returns an actionable error. Automatic
filename selection does not turn selected files into explicit user paths.
Storage failures and cancellation propagate rather than being treated as missing
indexes. Published writer sessions are reused through the shared read-access path.

Extracted matches retain source mappings. Overlapping entities are deduplicated
by source position before file-scoped `-m` counting; distinct source positions
remain distinct even when the text is identical. Freshness uses the same check
as indexed queries, once per file. Context expansion remains within an entity.
