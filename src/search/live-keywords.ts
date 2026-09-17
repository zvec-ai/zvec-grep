import { runRgSearch } from "../engine/service/lexical.js";
import { enrichLexicalItemsWithStructure } from "../engine/service/structure-enrichment.js";
import type {
  ZvecGrepContextItem,
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
} from "../engine/service/types.js";
import { searchIntent } from "../engine/pipeline/search/intent.js";

const KEYWORD_SCAN_LIMIT = 5_000;
const KEYWORD_CANDIDATE_LIMIT = 200;
const KEYWORD_SCAN_BUDGET_MS = 600;
const MAX_WINDOW_CHARS = 16_384;
const STOP_WORDS = new Set(
  "a an the and or of to in on with for from by at as is are was were be been being how what where when why does do did can could should would will that this these those it its they their them we you your".split(
    " ",
  ),
);

export type KeywordQuery = {
  /** Keep original word groups: one split identifier is not two query ideas. */
  groups: readonly (readonly string[])[];
  terms: readonly string[];
  patterns: readonly string[];
};

export function keywordQuery(query: string): KeywordQuery | undefined {
  if (query.length > 1_024 || searchIntent(query).kind !== "text")
    return undefined;
  // Script boundaries keep an adjacent Chinese question separate from its
  // embedded identifier. Do not manufacture a Chinese segmenter from chars.
  const atoms =
    query.match(/[\p{Script=Latin}\p{N}_$]+|\p{Script=Han}+|[\p{L}\p{M}]+/gu) ??
    [];
  const groups = atoms
    .map((atom) =>
      words(atom).filter((word) => word.length > 1 && !STOP_WORDS.has(word)),
    )
    .filter((group) => group.length > 0);
  const unique = [
    ...new Map(groups.map((group) => [group.join("\0"), group])).values(),
  ];
  const terms = [...new Set(unique.flat())];
  if (unique.length < 2 || unique.length > 12 || terms.length > 24)
    return undefined;
  return { groups: unique, terms, patterns: terms.map(termPattern) };
}

/** Match distinct words in one real source window, never a whole-file bag. */
export function keywordWindowScore(
  query: KeywordQuery,
  content: string,
): number {
  if (content.length > MAX_WINDOW_CHARS) return 0;
  const present = new Set(words(content));
  const supported = query.groups.filter((group) =>
    group.every((term) => present.has(term)),
  ).length;
  if (supported < 2 || supported / query.groups.length < 0.6) return 0;
  return supported / query.groups.length;
}

function words(text: string): string[] {
  return (
    text
      .replace(/([\p{Ll}\p{N}])(\p{Lu})/gu, "$1 $2")
      .replace(/(\p{Lu})(\p{Lu}\p{Ll})/gu, "$1 $2")
      .toLowerCase()
      .match(/[\p{Script=Latin}\p{N}]+|\p{Script=Han}+|[\p{L}\p{M}]+/gu) ?? []
  ).map((word) => {
    if (STOP_WORDS.has(word)) return word;
    if (/^[a-z]{3,}ies$/.test(word)) return `${word.slice(0, -3)}y`;
    if (/^[a-z]{3,}s$/.test(word) && !word.endsWith("ss"))
      return word.slice(0, -1);
    return word;
  });
}

function termPattern(term: string): string {
  const escaped = term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  if (/^[a-z]{3,}y$/.test(term)) return `${escaped.slice(0, -1)}(?:y|ies)`;
  // Substring discovery is deliberately wider than the final subword match.
  return escaped;
}

/** A +/- context window can cross a function boundary; score only its owner. */
export function keywordSourceWindow(
  item: ZvecGrepContextItem,
): ZvecGrepContextItem {
  const container = item.container?.range ?? item.excerptRange;
  if (item.range.kind !== "text" || container?.kind !== "text") return item;
  const startLine = Math.max(item.range.startLine, container.startLine);
  const endLine = Math.min(item.range.endLine, container.endLine);
  if (startLine > endLine) return { ...item, content: "" };
  if (startLine === item.range.startLine && endLine === item.range.endLine)
    return item;
  const lines = item.content.split("\n");
  const content = lines
    .slice(startLine - item.range.startLine, endLine - item.range.startLine + 1)
    .join("\n");
  return {
    ...item,
    content,
    range: {
      ...item.range,
      startLine,
      endLine,
      startOffset: 0,
      endOffset: content.split("\n").at(-1)?.length ?? 0,
    },
  };
}

type KeywordScope = {
  options: ZvecGrepContextOptions;
  displayPath(path: string): string;
};

/** Lazy fallback only: ordinary warm ranking and explicit rg are unchanged. */
export async function addLiveKeywordMatches(
  literal: ZvecGrepContextResult,
  scopes: readonly KeywordScope[],
  acceptsFile: (relativePath: string) => boolean,
  limit: number,
  signal?: AbortSignal,
): Promise<ZvecGrepContextResult> {
  signal?.throwIfAborted();
  const query = keywordQuery(literal.query);
  if (!query || literal.items.length >= limit) return literal;
  const candidates: ZvecGrepContextItem[] = [];
  let truncated = false;
  const deadline = performance.now() + KEYWORD_SCAN_BUDGET_MS;
  for (const scope of scopes) {
    signal?.throwIfAborted();
    const remaining = deadline - performance.now();
    if (remaining <= 0) {
      truncated = true;
      break;
    }
    const result = await runRgSearch({
      ...scope.options,
      root: literal.root,
      paths: scope.options.rgPaths,
      patterns: query.patterns,
      matchAllOccurrences: true,
      limit: KEYWORD_CANDIDATE_LIMIT,
      scanLimit: KEYWORD_SCAN_LIMIT,
      timeoutMs: remaining,
      signal,
      rgOptions: { ignoreCase: true, beforeContext: 2, afterContext: 2 },
      rankItem: (item) =>
        acceptsFile(scope.displayPath(item.file.absolutePath))
          ? keywordWindowScore(query, item.content)
          : 0,
    });
    truncated ||= result.diagnostics.truncated;
    const enriched = await enrichLexicalItemsWithStructure(
      literal.root,
      result.items,
      25,
      Math.min(scope.options.maxFileSizeBytes ?? Infinity, 1_048_576),
      true,
      { signal },
    );
    signal?.throwIfAborted();
    truncated ||= enriched.diagnostics.truncated;
    for (const raw of enriched.items) {
      const item = keywordSourceWindow(raw);
      const score = keywordWindowScore(query, item.content);
      if (score > 0)
        candidates.push({
          ...item,
          score,
          matchedBy: "keyword",
          file: {
            ...item.file,
            relativePath: scope.displayPath(item.file.absolutePath),
          },
        });
    }
  }
  candidates.sort((a, b) => b.score! - a.score! || a.rank - b.rank);
  const items = [...literal.items];
  // Score before collapsing a container, otherwise its first weak window wins.
  for (const item of candidates) {
    if (!items.some((existing) => sameTarget(existing, item))) items.push(item);
  }
  return {
    ...literal,
    coverage: "ranked_sample",
    items: items
      .slice(0, limit)
      .map((item, index) => ({ ...item, rank: index + 1 })),
    diagnostics: {
      ...literal.diagnostics,
      emptyReason: items.length ? undefined : literal.diagnostics.emptyReason,
      keywords: {
        terms: query.terms,
        truncated,
        candidates: candidates.length,
      },
    },
  };
}

function sameTarget(
  left: ZvecGrepContextItem,
  right: ZvecGrepContextItem,
): boolean {
  if (left.file.absolutePath !== right.file.absolutePath) return false;
  if (left.container && right.container)
    return left.container.entityId === right.container.entityId;
  // Several term anchors can produce the same unowned source window. Their
  // excerpt columns describe the matches, not distinct evidence to display.
  // Compare the actual returned text and line extent; known owners above still
  // keep different same-line functions separate.
  if (left.range.kind === "text" && right.range.kind === "text")
    return (
      left.range.startLine === right.range.startLine &&
      left.range.endLine === right.range.endLine &&
      left.content === right.content
    );
  return (
    JSON.stringify(left.range) === JSON.stringify(right.range) &&
    left.content === right.content
  );
}
