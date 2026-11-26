# PDF extraction

PDFs use LiteParse for native text and structure, with PP-DocLayout-S filtering
figure text while protecting native tables. Both parsing and page rendering use
the bundled PDFium library. Runtime use requires no Python or PDF viewer.

## Extraction boundary

Extraction prepares transient Markdown and source mappings, then reuses the
Markdown entity and fragment extractor. Indexing consumes those entities;
unchanged PDFs reuse stored content and mappings. Embedding and layout models
share the engine's artifact cache and runtime lifecycle.

OCR, image understanding, and precise table or formula reconstruction are not
supported. A PDF without searchable native text produces a warning and no
entities.

## Source locations

`PdfSourceMap` maps entity-local UTF-8 byte ranges to original pages and blocks.
It is stored alongside the entity's text. `PdfRange` contains only the resolved
pages and rectangles; it does not duplicate the text or source map.

Page numbers are one-based physical pages. Rectangles retain LiteParse's raw
`x`, `y`, `width`, and `height` in top-left, 72-DPI page coordinates (points),
along with each page's dimensions. Separate regions are preserved without
normalizing or merging them into a larger rectangle.

Indexed results report the entity's pages; [direct search](../../pipelines/direct_search/README.md)
reports the actual match pages, excluding context-only pages. Full previews
return the complete entity. CLI detail previews group raw boxes and page
dimensions by page without exposing internal block identifiers.

## Failure handling

PP-S loading or inference failures and page-rendering failures warn and fall
back to native text. Invalid predictions are discarded before text filtering.
PDFium is still required for native parsing. A file commits its complete
extraction or fails; other files continue indexing.

See [native builds](../../../../../docs/development/building.md#native-runtime-downloads-and-overrides)
for PDFium preparation and [validation](../../../../../docs/development/validation.md)
for extraction and packaged-runtime checks.
