// Convert wire representations without reproducing presentation logic.
const camel = (value) => {
  if (Array.isArray(value)) return value.map(camel);
  if (value === null) return undefined;
  if (typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value).map(([key, entry]) => [
      key.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase()),
      camel(entry),
    ]),
  );
};

const range = (value) =>
  value && {
    ...camel(value),
    ...(value.kind === "text"
      ? {
          startOffset: value.start_byte_offset,
          endOffset: value.end_byte_offset,
          endLine:
            value.end_line -
            Number(
              value.end_byte_column === 0 &&
                value.start_byte_offset < value.end_byte_offset,
            ),
        }
      : {}),
  };

const item = (value) => ({
  ...camel(value),
  file: {
    absolutePath: value.absolute_path,
    relativePath: value.relative_path,
  },
  range: range(value.range),
  excerptRange: range(value.excerpt_range),
  metadata:
    value.metadata?.kind === "code"
      ? { ...camel(value.metadata), modifiers: [] }
      : camel(value.metadata),
});

export const nodeResult = (value) => ({
  ...camel(value),
  items: value.items.map(item),
  groupResults: value.group_results.length
    ? value.group_results.map((group) => ({
        ...camel(group),
        items: group.items.map(item),
      }))
    : undefined,
});
