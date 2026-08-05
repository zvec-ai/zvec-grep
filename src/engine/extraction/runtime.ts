import type { EntityFragment } from "../types.js";
import { CodeExtractor } from "./code/extractor.js";
import { ImageExtractor } from "./image/extractor.js";
import { MarkdownExtractor } from "./markdown/extractor.js";
import type { Source } from "./source.js";
import { TextExtractor } from "./text/extractor.js";
import type { ChunkOptions } from "./types.js";

type ExtractorRoute = "code" | "image" | "markdown" | "text";

type SourceExtractor = {
  extract(source: Source, options?: ChunkOptions): Promise<EntityFragment[]>;
};

const extractors = {
  code: new CodeExtractor(),
  image: new ImageExtractor(),
  markdown: new MarkdownExtractor(),
  text: new TextExtractor(),
} satisfies Record<ExtractorRoute, SourceExtractor>;

export function extract(
  source: Source,
  options: ChunkOptions = {},
): Promise<EntityFragment[]> {
  return extractors[routeSource(source)].extract(source, options);
}

function routeSource(source: Source): ExtractorRoute {
  if (source.kind === "image") {
    return "image";
  }

  if (source.file.kind === "code") {
    return "code";
  }

  if (source.file.format === "markdown") {
    return "markdown";
  }

  return "text";
}
