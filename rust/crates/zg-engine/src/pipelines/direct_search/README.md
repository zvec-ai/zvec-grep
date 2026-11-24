# Direct search

Direct search serves `--rg` and the literal-search fallback after filename lookup.
Ordinary text files are searched directly with the embedded grep matcher; no
workspace index is required. Without a workspace index, PDFs use the same raw-file
matcher and binary policy as other files (`-a` explicitly enables binary search).
With an index, PDFs use canonical extracted text when available.

The coordinator owns requested scope, result ordering, and final limits. The
extracted reader returns hits and files without a usable index. Those files are
searched as raw content with an indexing hint, including explicitly requested
files. Index access failures also fall back to raw search, with a warning that
preserves the cause; cancellation and request errors still propagate. Raw hits
have file line/byte locations, never PDF pages or boxes. Search never builds or
refreshes an index. Published writer sessions reuse the shared read-access path.

Extracted matches retain source mappings. Overlapping entities are deduplicated
by source position before file-scoped `-m` counting; distinct source positions
remain distinct even when the text is identical. Freshness uses the same check
as indexed queries, once per file: stale extraction remains searchable with
`possibly_stale`, and its page locations refer to the indexed version. Context
expansion remains within an entity.
