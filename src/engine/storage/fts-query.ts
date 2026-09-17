/**
 * Query lexical terms without giving jieba whitespace/punctuation to recall.
 * matchString analyzes those characters too: even an absent two-word query can
 * match every document containing a space. Build a controlled OR expression;
 * user-supplied operators, wildcards and field syntax are never executable.
 *
 * ASCII terms are quoted so AND/OR/NOT remain ordinary words. Unicode terms
 * contain no parser syntax and stay unquoted so the native analyzer can recall
 * their constituent words (quoting a Chinese sentence would require a phrase).
 * Exact punctuation-sensitive matching belongs to live text search / rg.
 */
export function fullTextQuery(query: string): string | undefined {
  const words = query.match(/[\p{L}\p{N}][\p{L}\p{M}\p{N}]*/gu) ?? [];
  if (words.length === 0) return undefined;
  return [...new Set(words)]
    .map((word) => (/^[A-Za-z0-9]+$/.test(word) ? `"${word}"` : word))
    .join(" OR ");
}
