import type { Content, EntityFragment } from "../types.js";
import { CodeExtractor } from "./code/extractor.js";
import type {
  EntityOwnership,
  FunctionCallSites,
  SymbolRefSites,
  TypeInheritanceSites,
} from "./code/extractor.js";
import type { ImportSpec } from "./code/import-sites.js";
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

export type IndexingExtractionFragment = {
  fragment: EntityFragment;
  embeddingSource?: Content;
};

export async function extractForIndexing(
  source: Source,
  options: ChunkOptions = {},
): Promise<IndexingExtractionFragment[]> {
  if (routeSource(source) === "code") {
    return (await extractors.code.extractForIndexing(source, options)).map(
      ({ fragment, embeddingText }) => ({
        fragment,
        ...(embeddingText === undefined
          ? {}
          : { embeddingSource: { kind: "text", text: embeddingText } }),
      }),
    );
  }

  return (await extractors[routeSource(source)].extract(source, options)).map(
    (fragment) => ({ fragment }),
  );
}

export type PreparedCodeAnalysis = {
  fragments: IndexingExtractionFragment[];
  imports: readonly ImportSpec[];
  calls: readonly FunctionCallSites[];
  refs: readonly SymbolRefSites[];
  inheritance: readonly TypeInheritanceSites[];
  ownership: readonly EntityOwnership[];
};

export async function analyzeForIndexing(
  source: Source,
  options: ChunkOptions = {},
): Promise<PreparedCodeAnalysis> {
  if (routeSource(source) !== "code") {
    return {
      fragments: await extractForIndexing(source, options),
      imports: [],
      calls: [],
      refs: [],
      inheritance: [],
      ownership: [],
    };
  }
  const analysis = await extractors.code.analyzeForIndexing(source, options);
  return {
    ...analysis,
    fragments: analysis.fragments.map(({ fragment, embeddingText }) => ({
      fragment,
      ...(embeddingText === undefined
        ? {}
        : { embeddingSource: { kind: "text" as const, text: embeddingText } }),
    })),
  };
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
