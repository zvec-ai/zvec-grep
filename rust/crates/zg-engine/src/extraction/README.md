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

Markdown owns ATX/Setext recognition and code-fence handling. It converts section
offsets to source `TextRange` values, retains `MarkdownMetadata`, and uses the
existing metadata budget and `text_fragments` implementation. Long sections
remain complete entities with multiple retrieval fragments. The persisted
metadata shape and extraction routing do not change in this refactor.

A future LaTeX adapter can recognize sectioning commands and feed the same
splitter without converting the source to Markdown. DOCX or PDF adapters can
supply extracted text and headings from their document structure. Such offsets
refer to that extracted text, **not** to bytes in the original binary file.
Those adapters must supply original-document location mapping when constructing
entities; page, paragraph, or slide locations require a separate domain change.
The splitter deliberately returns text offsets rather than claiming a source
`Range`. It does not infer headings, perform OCR, or impose sectioning on sheets
and slides that need different semantic entities.

New formats should share the engine extraction path across server and direct
execution, and update structural-enrichment eligibility when appropriate.
Supporting converted documents in indexed search also requires an explicit
decision about direct lexical search over their extracted text. This refactor
adds no new file-format support and needs no index migration.

## Verification

The generic splitter tests use supplied heading positions, including inline
commands and arbitrary depths, without a Markdown parser. Markdown compatibility
snapshots were captured from the original extractor and cover LF/CRLF, Unicode,
hierarchy, repeated headings, fences, fallback, retrieval ranges, entity IDs,
metadata, and embedding inputs. Transport tests compare Markdown previews in
direct and server modes, including full entities spanning multiple fragments.
