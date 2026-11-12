import assert from "node:assert/strict";
import {
  scoreResponse as evaluateResponse,
  VisibleFormatError,
  relativePath,
} from "../../core/response.mjs";
export { VisibleFormatError };

/**
 * Locate the full returned Entity in the verified source. Compact MCP headers
 * only expose line numbers. A unique alignment supplies the missing columns.
 */
export function locateFullEntity(item, file) {
  if (!file || item.range?.kind !== "text") return null;
  const { start_line, end_line } = item.range;
  if (end_line > file.lines.length) return null;
  const visible = item.source_lines.filter(({ line }) => line <= end_line);
  if (visible.length !== end_line - start_line + 1) return null;
  for (const [i, row] of visible.entries()) {
    if (row.line !== start_line + i) return null;
    const text = file.lines[row.line - 1].text;
    if (i > 0 && i < visible.length - 1 && row.text !== text) return null;
  }
  const first = file.lines[start_line - 1];
  const last = file.lines[end_line - 1];
  const head = visible[0].text;
  const tail = visible.at(-1).text;
  if (first === last) {
    const start = first.text.indexOf(head);
    if (!head || start < 0 || first.text.indexOf(head, start + 1) >= 0)
      return null;
    const startByte =
      first.offset + Buffer.byteLength(first.text.slice(0, start));
    return {
      start_byte: startByte,
      end_byte: startByte + Buffer.byteLength(head),
    };
  }
  if (!first.text.endsWith(head) || !last.text.startsWith(tail)) return null;
  return {
    start_byte:
      first.offset +
      Buffer.byteLength(first.text.slice(0, first.text.length - head.length)),
    end_byte: last.offset + Buffer.byteLength(tail),
  };
}

/** Validate reported indexed routes; compact output explicitly leaves them unknown. */
export function validateSearchRoute(items, mode) {
  const allowed =
    mode === "hybrid"
      ? ["fts", "vector", "fts+vector"]
      : ["fts", "vector"].includes(mode)
        ? [mode]
        : null;
  assert.ok(allowed, `unknown search mode: ${String(mode)}`);
  assert.ok(Array.isArray(items), `${mode}: missing public parsed items`);
  for (const item of items)
    assert.ok(
      item?.matched_by === null || allowed.includes(item?.matched_by),
      `${mode}: unexpected matchedBy=${String(item?.matched_by)} at rank ${item?.rank ?? "?"}; expected ${allowed.join(" or ")}`,
    );
}

function fail(message) {
  throw new VisibleFormatError(message);
}

function visibleText(response) {
  if (
    !response ||
    !Array.isArray(response.content) ||
    response.content.length !== 1 ||
    response.content[0]?.type !== "text" ||
    typeof response.content[0].text !== "string"
  ) {
    fail("Expected the public search response's single text content block.");
  }
  return response.content[0].text;
}

function parseRange(text) {
  const match = /^([1-9]\d*)(?:-([1-9]\d*))?$/.exec(text);
  if (match) {
    const start = Number(match[1]);
    const end = Number(match[2] ?? match[1]);
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(end) ||
      end < start
    )
      fail("Invalid source range.");
    return { kind: "text", start_line: start, end_line: end };
  }
  if (/^(?:file|page:[1-9]\d*|bytes:\d+-\d+)$/.test(text))
    return { kind: "other", label: text };
  fail(`Unknown range: ${text}`);
}

function parseCompactResponse(text, lines, freshness) {
  while (lines.at(-1) === "") lines.pop();
  if (lines[0] === "status: possibly_stale") {
    if (freshness === "fresh")
      fail("Global diagnostics conflict with freshness.");
    lines.shift();
    if (lines[0] === "results: served_from_current_index") {
      lines.shift();
      if (!/^background_refresh: \S.*$/.test(lines.shift() ?? ""))
        fail("Missing background refresh status.");
    }
    while (lines[0] === "") lines.shift();
  } else {
    const status = lines.lastIndexOf("status: possibly_stale");
    if (status >= 0) {
      const diagnostics = lines.slice(status);
      if (
        diagnostics.length === 1 ||
        (diagnostics.length === 3 &&
          diagnostics[1] === "results: served_from_current_index" &&
          /^background_refresh: \S.*$/.test(diagnostics[2]))
      ) {
        if (freshness === "fresh")
          fail("Global diagnostics conflict with freshness.");
        lines.splice(status);
      }
    }
  }
  // An empty string is not a successful empty search. The product has explicit empty labels.
  if (lines[0] === "No matches." || lines[0] === "No searchable files.") {
    const emptyReason =
      lines.shift() === "No matches." ? "no_matches" : "no_searchable_files";
    if (lines[0]?.startsWith("missing: ")) lines.shift();
    if (lines.some((line) => line !== ""))
      fail("Unexpected text after empty result.");
    return { text, items: [], freshness, empty_reason: emptyReason };
  }
  const header =
    /^#([1-9]\d*) (?!matchedBy=)(.+?):((?:[1-9]\d*(?:-[1-9]\d*)?)|file|page:[1-9]\d*|bytes:\d+-\d+)$/;
  const items = [];
  let item;
  let sourceStarted = false;
  for (const line of lines) {
    const match = header.exec(line);
    if (match) {
      const rank = Number(match[1]);
      if (!Number.isSafeInteger(rank) || rank !== items.length + 1 || rank > 10)
        fail("Expected consecutive native top-10 ranks.");
      if (!relativePath(match[2]))
        fail("Expected an unambiguous repository-relative path.");
      item = {
        rank,
        path: match[2],
        range: parseRange(match[3]),
        matched_by: null,
        header: line,
        metadata: [],
        outline: [],
        source_lines: [],
        unnumbered_source: [],
        raw_lines: [line],
        matched_range: null,
      };
      items.push(item);
      sourceStarted = false;
      continue;
    }
    if (!item) {
      if (line === "") continue;
      fail("Unknown public search output before the first ranked item.");
    }
    item.raw_lines.push(line);
    if (line === "" || line === "  …") continue;
    if (
      !sourceStarted &&
      /^(?:status: possibly_stale$|symbol: |heading: |heading_level: \d+$|scope: )/.test(
        line,
      )
    ) {
      item.metadata.push(line);
      continue;
    }
    const source = /^ {2}([1-9]\d*): (.*)$/.exec(line);
    if (source && item.range.kind === "text") {
      const number = Number(source[1]);
      if (
        !Number.isSafeInteger(number) ||
        number <= (item.source_lines.at(-1)?.line ?? 0)
      )
        fail("Repeated or unordered visible source lines.");
      if (number < item.range.start_line || number > item.range.end_line)
        fail("Visible source falls outside the item's public range.");
      item.source_lines.push({ line: number, text: source[2] });
      sourceStarted = true;
      continue;
    }
    if (line.startsWith("  ") && item.range.kind === "other") {
      item.unnumbered_source.push(line.slice(2));
      sourceStarted = true;
      continue;
    }
    fail(`Unknown line in result #${item.rank}: ${line.slice(0, 80)}`);
  }
  if (items.length === 0)
    fail("Missing explicit empty result or ranked items.");
  return { text, items, freshness, empty_reason: null };
}

/** Parse only the public, single-query MCP search text. Never consult hidden structuredContent. */
export function parseVisibleResponse(response) {
  const text = visibleText(response);
  if (text.includes("\x1b") || text.includes("\r"))
    fail("Unexpected terminal escapes or line framing.");
  const lines = text.split("\n");
  const first =
    /^freshness: (fresh|possibly_stale|served_from_current_index)$/.exec(
      lines.shift() ?? "",
    );
  if (!first) fail("Missing public MCP freshness header.");
  if (lines[0] === "results: served_from_current_index") {
    lines.shift();
    if (!/^background_refresh: \S.*$/.test(lines.shift() ?? ""))
      fail("Missing background refresh status.");
  } else if (lines[0]?.startsWith("background_refresh: ")) {
    if (first[1] !== "served_from_current_index")
      fail("Unexpected background refresh status.");
    lines.shift();
  }
  const items = [];
  let item;
  let section = "metadata";
  const header =
    /^#([1-9]\d*)(?: \[(?:group_coverage: [^\]\n]+|global_fill)\])? matchedBy=(fts\+vector|fts|vector|lexical)(?: score=(-?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?))? (.+?):((?:[1-9]\d*(?:-[1-9]\d*)?)|file|page:[1-9]\d*|bytes:\d+-\d+)$/;
  if (!lines.some((line) => header.test(line)))
    return parseCompactResponse(text, lines, first[1]);
  for (const line of lines) {
    const match = header.exec(line);
    if (match) {
      const rank = Number(match[1]);
      if (!Number.isSafeInteger(rank) || rank !== items.length + 1 || rank > 10)
        fail("Expected consecutive native top-10 ranks.");
      if (!relativePath(match[4]))
        fail("Expected an unambiguous repository-relative path.");
      item = {
        rank,
        path: match[4],
        range: parseRange(match[5]),
        matched_by: match[2],
        header: line,
        metadata: [],
        outline: [],
        source_lines: [],
        unnumbered_source: [],
        raw_lines: [line],
        matched_range: null,
      };
      items.push(item);
      section = "metadata";
      continue;
    }
    if (!item) {
      if (line === "") continue;
      fail("Unknown public search output before the first ranked item.");
    }
    item.raw_lines.push(line);
    if (line === "source:") {
      section = "source";
      continue;
    }
    if (line === "outline:" && section === "metadata") {
      section = "outline";
      continue;
    }
    if (line.startsWith("outline: ") && section === "metadata") {
      item.outline.push(line.slice("outline: ".length));
      continue;
    }
    if (/^matched: /.test(line)) {
      item.matched_range = parseRange(line.slice(9));
      continue;
    }
    if (
      section === "metadata" &&
      /^(?:groups: |status: possibly_stale$|symbol: |heading: |heading_level: \d+$|scope: )/.test(
        line,
      )
    ) {
      item.metadata.push(line);
      continue;
    }
    const source = /^([1-9]\d*)(?::|-)?\t(.*)$/.exec(line);
    if (source) {
      const number = Number(source[1]);
      if (
        !Number.isSafeInteger(number) ||
        number <= (item.source_lines.at(-1)?.line ?? 0)
      )
        fail("Repeated or unordered visible source lines.");
      // The public Rust/Node presentation numbers a terminal newline as one
      // empty line after the advertised content range.
      if (
        item.range.kind === "text" &&
        (number < item.range.start_line || number > item.range.end_line) &&
        !(number === item.range.end_line + 1 && source[2] === "")
      )
        fail("Visible source falls outside the item's public range.");
      item.source_lines.push({ line: number, text: source[2] });
      section = "source";
      continue;
    }
    if (
      line.startsWith("\t") &&
      item.range.kind === "other" &&
      section !== "outline"
    ) {
      item.unnumbered_source.push(line.slice(1));
      section = "source";
      continue;
    }
    if (line.startsWith("  ") && section === "source") {
      item.unnumbered_source.push(line.slice(2));
      continue;
    }
    if (line === "" || line === "...") {
      if (section === "outline") item.outline.push(line);
      continue;
    }
    if (section === "outline") {
      item.outline.push(line);
      continue;
    }
    fail(`Unknown line in result #${item.rank}: ${line.slice(0, 80)}`);
  }
  if (items.length === 0)
    fail("Missing explicit empty result or ranked items.");
  return { text, items, freshness: first[1], empty_reason: null };
}

export function scoreResponse(response, gold, options = {}) {
  return evaluateResponse(response, gold, {
    parseResponse: parseVisibleResponse,
    ...options,
  });
}
