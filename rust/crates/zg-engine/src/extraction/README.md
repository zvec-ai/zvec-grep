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
Long sections remain complete entities with multiple retrieval fragments.

For converted documents, section offsets refer to the extracted UTF-8 text.
The adapter maps those offsets to original document locations when constructing
entities and defines how to store and return those locations in search results.

New formats should share the engine extraction path across server and direct
execution, and update structural-enrichment eligibility when appropriate.
Supporting converted documents in indexed search also requires an explicit
decision about direct lexical search over their extracted text.

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

### Compatibility with PDF extraction

The PDF adapter builds transient Markdown and a `PdfSourceMap`. Only whitespace
separators may lack a source location. The adapter runs
`markdown::extract` and uses each entity's text range to slice the map and
resolve its PDF pages and blocks. See [PDF extraction](pdf/README.md) for
coordinate conventions and failure handling.

## Verification

The generic splitter tests use supplied heading positions, including inline
commands and arbitrary depths, without a Markdown parser. Markdown compatibility
snapshots cover LF/CRLF, Unicode,
hierarchy, repeated headings, fences, fallback, retrieval ranges, entity IDs,
metadata, and embedding inputs. Transport tests compare Markdown previews in
direct and server modes, including full entities spanning multiple fragments.
Snapshot object keys are recursively sorted before hashing, so workspace builds
that enable `serde_json/preserve_order` keep the same engine-only baselines.
Source-map tests cover UTF-8 and CRLF boundaries, invalid inputs, overlapping
origins, generated gaps, empty selections, repeated text, and entity/fragment
composition. A converted-text adapter test maps both structured Markdown and
its plain-text fallback after extraction, without changing the parser interface.
