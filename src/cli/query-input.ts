/**
 * The shell has already removed quoting by the time argv reaches us. Ordinary
 * positional words describe one search, whether the user quoted them or not.
 * Multiple query groups are an explicit choice via repeated --hybrid options.
 * Managed rg has its own pattern/path grammar and must not use this normalizer.
 */
export function normalizeCliQueries(
  positionals: readonly string[],
  hybridQueries: readonly string[] = [],
): string[] {
  return [positionals.join(" "), ...hybridQueries]
    .map((query) => query.trim())
    .filter((query) => query.length > 0);
}
