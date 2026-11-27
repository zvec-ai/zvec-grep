# Direct search

Direct search serves `--rg` and the literal-search fallback after filename lookup.
It searches ordinary files with the embedded grep matcher. PDFs use extracted
text from their nearest workspace when available; otherwise they use raw-file
search and its binary policy (`-a` enables binary search). Search never builds
or refreshes an index.

## Matches and limits

- Extracted hits use source mappings for page locations. Stale extraction remains
  searchable with `possibly_stale`; locations refer to the indexed version.
- Overlapping entity matches are deduplicated by source position before applying
  the per-file `-m` limit. Identical text at different positions stays distinct.
- Global output limits count matching records before entity grouping, across
  ordinary files and extracted sources. Context does not consume that limit;
  extracted context stays within its entity.

## Fallback

Within an indexed workspace, missing extraction or index access failures warn
and fall back to raw-file search, including for explicit file paths. Diagnostics
retain the failure cause and an indexing hint. Cancellation and request errors
still propagate. Raw hits have file line/byte locations, never PDF pages or boxes.
