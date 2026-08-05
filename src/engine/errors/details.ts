type ErrorDetailValue = string | number | boolean | null | undefined;

export type ErrorDetailEntry =
  string | readonly [string, ErrorDetailValue] | null | undefined;

export function errorDetails(
  entries: readonly ErrorDetailEntry[],
): string | undefined {
  const lines = entries
    .map(formatDetailEntry)
    .filter((line): line is string => line !== null && line.length > 0);

  return lines.length > 0 ? lines.join("\n") : undefined;
}

export function detail(key: string, value: ErrorDetailValue): ErrorDetailEntry {
  return value === null || value === undefined ? null : [key, value];
}

export function collectionDetail(name: string): ErrorDetailEntry {
  return detail("collection", name);
}

function formatDetailEntry(entry: ErrorDetailEntry): string | null {
  if (entry === null || entry === undefined) {
    return null;
  }

  if (typeof entry === "string") {
    return entry.trim().length > 0 ? entry.trim() : null;
  }

  const [key, value] = entry;
  if (value === null || value === undefined) {
    return null;
  }

  return `${key}=${String(value)}`;
}
