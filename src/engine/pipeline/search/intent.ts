import type { Entity, EntityFragment, FileInfo } from "../../types.js";

export type SearchIntent = {
  query: string;
  kind: "identifier" | "path" | "text";
};

/** Cheap hints, not an exclusive router: ambiguous queries retain all routes. */
export function searchIntent(query: string): SearchIntent {
  const text = query.trim();
  if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(text)) {
    return { query: text, kind: "identifier" };
  }
  if (
    !/\s/.test(text) &&
    !text.includes("://") &&
    (text.includes("/") || /\.[A-Za-z0-9]{1,12}$/.test(text)) &&
    !/[?*<>|]/.test(text)
  ) {
    return { query: text.replace(/^\.\//, ""), kind: "path" };
  }
  return { query: text, kind: "text" };
}

/**
 * Exact evidence is a ranking tier, not a calibrated confidence score. In
 * particular, a mixed question mentioning a symbol is not a symbol lookup.
 */
export function exactMatchPriority(
  intent: SearchIntent,
  entity: Entity,
  file: FileInfo,
  fragments: readonly EntityFragment[] = [],
): number {
  const { query, kind } = intent;
  if (kind === "identifier" && entity.metadata?.kind === "code") {
    if (entity.metadata.symbolName === query) return 3;
  }
  if (kind === "path") {
    const path = file.relativePath.replaceAll("\\", "/");
    if (path === query || path.endsWith(`/${query}`)) return 3;
  }
  if (query.length < 2) return 0;
  // A grouped entity may only contain an outline. Literal evidence lives in
  // recalled source fragments; do not concatenate unrelated fragments into a
  // synthetic match, or use fragment metadata as a declaration-name shortcut.
  const contents = [entity, ...fragments].flatMap((item) =>
    item.content.kind === "text" ? [item.content.text] : [],
  );
  if (kind === "identifier") {
    const escaped = query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const pattern = new RegExp(
      `(?:^|[^\\p{L}\\p{N}_$])${escaped}(?=$|[^\\p{L}\\p{N}_$])`,
      "u",
    );
    return contents.some((content) => pattern.test(content)) ? 2 : 0;
  }
  return contents.some((content) => content.includes(query)) ? 1 : 0;
}
