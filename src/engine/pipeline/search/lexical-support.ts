import type { Entity, EntityFragment, FileInfo } from "../../types.js";

type CandidateText = {
  id: string;
  entity: Entity;
  file: FileInfo;
  fragments: readonly EntityFragment[];
};

const MAX_QUERY_CHARS = 8_192;
const MAX_QUERY_TERMS = 64;
const MAX_WINDOW_CHARS = 32_768;
const STOP_WORDS = new Set(
  "a an the and or of to in on with for from by at as is are was were be been being how what where when why does do did can could should would will that this these those it its they their them we you your after before than then into across".split(
    " ",
  ),
);

/**
 * A bounded ranking hint, not exact-match evidence or semantic confidence.
 * Native FTS does not expose all identifier subwords. Use names, paths and
 * individual recalled text windows to recognize that support at read time.
 * Never concatenate unrelated windows into an invented complete match.
 */
export function lexicalSupport(
  candidates: readonly CandidateText[],
  query: string,
): Map<string, number> {
  if (query.length > MAX_QUERY_CHARS || candidates.length === 0)
    return new Map();
  const terms = [
    ...new Set(words(query).filter((word) => !STOP_WORDS.has(word))),
  ];
  if (terms.length === 0 || terms.length > MAX_QUERY_TERMS) return new Map();

  const exact = new Map(terms.map((term, i) => [term, i]));
  const prefixes = new Map<string, { term: string; index: number }[]>();
  for (const [index, term] of terms.entries()) {
    if (!/^[a-z]{4,}$/.test(term)) continue;
    const key = term.slice(0, 4);
    const bucket = prefixes.get(key) ?? [];
    bucket.push({ term, index });
    prefixes.set(key, bucket);
  }
  const match = (text: string): boolean[] => {
    const present = terms.map(() => false);
    for (const token of new Set(words(text))) {
      const index = exact.get(token);
      if (index !== undefined) present[index] = true;
      for (const prefix of prefixes.get(token.slice(0, 4)) ?? [])
        if (token.startsWith(prefix.term)) present[prefix.index] = true;
    }
    return present;
  };
  const records = candidates.map((candidate) => {
    const metadata = candidate.entity.metadata;
    const name =
      metadata?.kind === "code" ? metadata.symbolName : metadata?.heading;
    const anchor = match(
      `${candidate.file.relativePath} ${name ?? ""} ${metadata?.scope ?? ""}`,
    );
    const seen = new Set<string>();
    const windows = [candidate.entity, ...candidate.fragments].flatMap(
      (item) => {
        if (item.content.kind !== "text" || seen.has(item.id)) return [];
        seen.add(item.id);
        return [match(item.content.text)];
      },
    );
    return { id: candidate.id, anchor, windows };
  });
  // Discount terms shared by many candidates rather than rewarding repetition
  // in a long class/outline. This is candidate-set frequency, not corpus BM25.
  const weights = terms.map(
    (_, i) =>
      1 +
      Math.log(
        (records.length + 1) /
          (records.filter(
            (record) =>
              record.anchor[i] || record.windows.some((window) => window[i]),
          ).length +
            1),
      ),
  );
  const total = weights.reduce((sum, weight) => sum + weight, 0);
  const coverage = (present: readonly boolean[]) =>
    weights.reduce((sum, weight, i) => sum + (present[i] ? weight : 0), 0) /
    total;
  return new Map(
    records.map((record) => [
      record.id,
      (coverage(record.anchor) +
        record.windows.reduce(
          (best, window) => Math.max(best, coverage(window)),
          0,
        )) /
        2,
    ]),
  );
}

function words(text: string): string[] {
  return (
    text
      .slice(0, MAX_WINDOW_CHARS)
      .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
      .replace(/([A-Z])([A-Z][a-z])/g, "$1 $2")
      .toLowerCase()
      .match(/[\p{L}\p{N}]+/gu) ?? []
  ).map((word) => {
    if (word.length > 4 && word.endsWith("ies")) return word.slice(0, -3) + "y";
    if (word.length > 3 && word.endsWith("s") && !word.endsWith("ss"))
      return word.slice(0, -1);
    return word;
  });
}
