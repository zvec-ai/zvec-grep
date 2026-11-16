# Heading-based extraction

`sectioning::split_sections` is the shared, format-independent section splitter.
It accepts UTF-8 text and recognized headings in source order, and returns
non-overlapping content ranges, heading references, and ancestor titles. It has
no filesystem access, format detection, parser dependency, or storage effects.

Each format adapter supplies a heading's title, positive level, section start,
and preceding section end. Usually the two offsets are equal. An adapter can
exclude the whitespace between them when its format requires it. Markdown
excludes exactly the LF before a heading, retaining CR in CRLF input to preserve
existing entity content, coordinates, and IDs. Offsets are validated before
slicing; separators cannot discard non-whitespace content.

Every heading starts a section that ends before the next heading, irrespective
of level. Ancestors describe context without duplicating their children's
content. Nonblank preambles get an untitled section; blank preambles are omitted.
An empty heading list returns no sections so the caller can retain its existing
plain-text fallback. Repeated titles, skipped levels, levels beyond six, and
headings in the middle of a line are supported.

## Adapters and entity construction

Markdown owns ATX/Setext recognition and code-fence handling. Its `TextSourceMap`
converts section offsets to source `TextRange` values, retains the existing line
and byte-column conventions, and shares its line index with heading recognition.
The adapter retains `MarkdownMetadata` and uses the existing metadata budget and
`text_fragments` implementation. Long sections
remain complete entities with multiple retrieval fragments. The persisted
metadata shape and extraction routing do not change in this refactor.

A future LaTeX adapter can recognize sectioning commands and feed the same
splitter without converting the source to Markdown. DOCX or PDF adapters can
supply extracted text and headings from their document structure. Such offsets
refer to that extracted text, **not** to bytes in the original binary file.
Those adapters must supply original-document location mapping when constructing
entities. They can opt into the shared source-map module or retain their own
mapping implementation; persisting page, paragraph, or slide locations requires
a separate domain change.
The splitter deliberately returns text offsets rather than claiming a source
`Range`. It does not infer headings, perform OCR, or impose sectioning on sheets
and slides that need different semantic entities.

New formats should share the engine extraction path across server and direct
execution, and update structural-enrichment eligibility when appropriate.
Supporting converted documents in indexed search also requires an explicit
decision about direct lexical search over their extracted text. This refactor
adds no new file-format support and needs no index migration.

## Optional source mapping

`source_map::SourceMap` maps a half-open interval in its associated UTF-8 text to
an adapter-defined `Selection`. Mapping remains separate from `split_sections`:
call `map_range(section.range)` when constructing entities, then retain the
mapping needed to locate retrieval fragments. Implementations bind the map to
immutable text and reject reversed, out-of-bounds, and non-character boundaries.
Empty selections are valid at character boundaries, including EOF.

- `TextSourceMap` owns the line index for borrowed decoded text and returns an
  exact `TextRange`. Its offsets describe decoded text, which need not have the
  same encoding or byte positions as the original file.
- `SegmentSourceMap<L>` maps converted text to opaque locations supplied by the
  adapter, such as document blocks, paragraphs, or slides. Construct it over the
  complete parsed document with `SourcePart { text_range, location }` values in
  start-offset order. Mapping returns another `SegmentSourceMap` containing the
  selected text and intersecting parts rebased to that text. `text_range()` keeps
  the selection's position in the original parsed document, so identical text
  at different positions remains distinguishable.

The segment mapper supports repeated locations and overlapping parts for text
with multiple origins. It allows unmapped gaps, including generated text; source
coverage requirements and location validation belong to each format adapter.
An empty selection has no source parts and does not guess a nearby location.
Clipping a part preserves its entire location: selecting part of a block does
not invent character boxes or imply affine offsets in the original source.
Adapters needing precise source-text offsets should use exact text mapping or
provide their own `SourceMap` implementation.

An entity selection can be mapped again using entity-relative fragment offsets.
Selecting a fragment through its entity must produce the same text, original
document interval, and source parts as selecting it directly from the full map.
The segment implementation is available for future converted-format adapters;
ordinary Markdown uses only `TextSourceMap` and pays no per-segment cost.

### Compatibility with PDF extraction

The PDF work in PR #386 can keep its existing sequence: assemble transient
Markdown and `PdfSourceMap`, call `markdown::extract`, then use the returned
`Range::Text` coordinates to slice its own map into `Range::Pdf`. Markdown's
entry point and text-range output remain unchanged, including the unstructured
fallback. There is no mandatory source-map field on `TextSource` or entities.

The generic module does not replace `PdfSourceMap`, its whitespace-only coverage
rule, page/block validation, serialization, or search behavior. Both PRs remain
independent. An optional later adapter could implement the trait by delegating
to PDF's existing `slice`; sharing its internals or migrating stored mappings
would be separate changes with PDF-specific regression coverage. New converted
formats must separately define how locations are persisted and returned by
server/direct search; this extraction-only interface does not supply that path.

## Verification

The generic splitter tests use supplied heading positions, including inline
commands and arbitrary depths, without a Markdown parser. Markdown compatibility
snapshots were captured from the original extractor and cover LF/CRLF, Unicode,
hierarchy, repeated headings, fences, fallback, retrieval ranges, entity IDs,
metadata, and embedding inputs. Transport tests compare Markdown previews in
direct and server modes, including full entities spanning multiple fragments.
Snapshot object keys are recursively sorted before hashing, so workspace builds
that enable `serde_json/preserve_order` keep the same engine-only baselines.
Source-map tests cover UTF-8 and CRLF boundaries, invalid inputs, overlapping
origins, generated gaps, empty selections, repeated text, and entity/fragment
composition. A converted-text adapter test maps both structured Markdown and
its plain-text fallback after extraction, without changing the parser interface.
