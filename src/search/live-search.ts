import type {
  ZvecGrepContextItem,
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
} from "../engine/service/types.js";
import { searchIntent } from "../engine/pipeline/search/intent.js";

export const LIVE_CANDIDATE_LIMIT = 200;

export function isExactIdentifierQuery(query: string): boolean {
  // A capitalized ordinary word can still be a concept. Internal word breaks,
  // underscores and digits are stronger evidence of an actual code lookup.
  return (
    searchIntent(query).kind === "identifier" &&
    /[a-z\d][A-Z]|_|[A-Za-z]\d/.test(query) &&
    !query.includes("$")
  );
}

export function hasCompleteLiveLookup(result: ZvecGrepContextResult): boolean {
  return (
    (isExactIdentifierQuery(result.query) ||
      searchIntent(result.query).kind === "path") &&
    result.coverage === "rg_exhaustive"
  );
}

export function livePathPriority(
  query: string,
  relativePath: string,
  absolutePath: string,
): number {
  const path = query.replaceAll("\\", "/").replace(/^\.\//, "");
  if (relativePath === path || absolutePath.replaceAll("\\", "/") === path)
    return 3;
  return relativePath.endsWith(`/${path}`) ? 2 : 0;
}

export function liveSearchOptions(
  query: string,
  options: ZvecGrepContextOptions,
): ZvecGrepContextOptions {
  const intent = searchIntent(query);
  return {
    ...options,
    queries: [query],
    query: undefined,
    routes: undefined,
    rg: true,
    rgOptions: {
      fixedStrings: true,
      wordRegexp: intent.kind === "identifier",
      ignoreCase: query === query.toLowerCase(),
      beforeContext: 2,
      afterContext: 2,
    },
    limit: LIVE_CANDIDATE_LIMIT,
    autoUpdate: false,
  };
}

export function rankLiveResults(
  result: ZvecGrepContextResult,
  limit: number,
): ZvecGrepContextResult {
  const intent = searchIntent(result.query);
  const priority = (item: ZvecGrepContextItem): number =>
    intent.kind === "identifier" &&
    item.metadata?.kind === "code" &&
    item.metadata.symbolName === intent.query
      ? 1
      : 0;
  const items = [...result.items].sort(
    (a, b) => priority(b) - priority(a) || a.rank - b.rank,
  );
  const seen = new Set<string>();
  const unique = items.filter((item) => {
    const key =
      item.container?.entityId ??
      `${item.file.absolutePath}:${JSON.stringify(item.range)}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  const selected = unique
    .slice(0, limit)
    .map((item, index) => ({ ...item, rank: index + 1 }));
  return {
    ...result,
    items: selected,
    coverage:
      result.coverage === "rg_truncated" || unique.length > selected.length
        ? "rg_truncated"
        : result.coverage,
  };
}

export function hasStrongLiveMatch(result: ZvecGrepContextResult): boolean {
  const intent = searchIntent(result.query);
  // Common words and phrases are ambiguous. Only a complete code-shaped
  // identifier with a verified declaration can end search early.
  return (
    intent.kind === "identifier" &&
    /[A-Z_$]|[a-z][0-9]/.test(intent.query) &&
    result.items.some(
      (item) =>
        item.metadata?.kind === "code" &&
        item.metadata.symbolName === intent.query,
    )
  );
}
