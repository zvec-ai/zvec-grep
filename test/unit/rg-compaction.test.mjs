import assert from "node:assert/strict";
import test from "node:test";
import {
  compactRgContextItems,
  MAX_RG_GROUP_OCCURRENCE_SAMPLES,
} from "../../dist/engine/service/rg-compaction.js";

function textRange(startLine, endLine = startLine) {
  return {
    kind: "text",
    startLine,
    endLine,
    startOffset: 0,
    endOffset: 20,
  };
}

function lexicalItem({
  rank,
  file = "src/example.ts",
  matchLine,
  contextStart = matchLine,
  contextEnd = matchLine,
  containerId,
  containerStart = 1,
  containerEnd = 100,
}) {
  const range = textRange(contextStart, contextEnd);
  const excerptRange = textRange(matchLine);
  const item = {
    kind: "lexical_match",
    rank,
    file: {
      absolutePath: `/repo/${file}`,
      relativePath: file,
      rootPath: "/repo",
    },
    range,
    excerptRange,
    content: Array.from(
      { length: contextEnd - contextStart + 1 },
      (_, index) => `line ${contextStart + index}`,
    ).join("\n"),
    status: "fresh",
    matchedBy: "lexical",
  };

  if (containerId) {
    item.container = {
      entityId: containerId,
      range: textRange(containerStart, containerEnd),
    };
  }
  return item;
}

test("compacts every occurrence in the same structural container", () => {
  const result = compactRgContextItems({
    items: [
      lexicalItem({
        rank: 1,
        matchLine: 12,
        contextStart: 10,
        contextEnd: 14,
        containerId: "function:prepare",
        containerStart: 8,
        containerEnd: 20,
      }),
      lexicalItem({
        rank: 2,
        matchLine: 15,
        contextStart: 13,
        contextEnd: 17,
        containerId: "function:prepare",
        containerStart: 8,
        containerEnd: 17,
      }),
      lexicalItem({
        rank: 3,
        matchLine: 18,
        contextStart: 16,
        contextEnd: 20,
        containerId: "function:prepare",
        containerStart: 16,
        containerEnd: 20,
      }),
    ],
  });

  assert.equal(result.items.length, 1);
  assert.deepEqual(
    result.items[0].occurrences.map(
      (occurrence) => occurrence.excerptRange.startLine,
    ),
    [12, 15, 18],
  );
  assert.deepEqual(result.items[0].container.range, textRange(8, 20));
  assert.deepEqual(result.diagnostics, {
    rawOccurrences: 3,
    uniqueOccurrences: 3,
    exactDuplicatesRemoved: 0,
    groupsFound: 1,
    groupsReturned: 1,
    occurrencesCollapsed: 2,
    contextWindowsMerged: 2,
    groupTruncated: false,
  });
});

test("uses the stable entity id within each canonical file", () => {
  const result = compactRgContextItems({
    items: [
      lexicalItem({
        rank: 1,
        matchLine: 10,
        containerId: "function:run",
        containerStart: 8,
        containerEnd: 12,
      }),
      lexicalItem({
        rank: 2,
        file: "src/other.ts",
        matchLine: 10,
        containerId: "function:run",
        containerStart: 8,
        containerEnd: 12,
      }),
      lexicalItem({
        rank: 3,
        matchLine: 30,
        containerId: "function:other",
        containerStart: 28,
        containerEnd: 32,
      }),
    ],
  });

  assert.equal(result.items.length, 3);
  assert.equal(
    result.items.every((item) => item.occurrences === undefined),
    true,
  );
});

test("falls back to connected context windows without a container", () => {
  const result = compactRgContextItems({
    items: [
      lexicalItem({
        rank: 1,
        matchLine: 11,
        contextStart: 10,
        contextEnd: 12,
      }),
      lexicalItem({
        rank: 2,
        matchLine: 14,
        contextStart: 13,
        contextEnd: 15,
      }),
      lexicalItem({
        rank: 3,
        matchLine: 30,
        contextStart: 29,
        contextEnd: 31,
      }),
    ],
  });

  assert.equal(result.items.length, 2);
  assert.equal(result.items[0].occurrences.length, 2);
  assert.deepEqual(result.items[0].range, textRange(10, 15));
  assert.equal(result.items[1].occurrences, undefined);
  assert.equal(result.diagnostics.contextWindowsMerged, 1);
});

test("removes exact canonical occurrence duplicates before grouping", () => {
  const duplicate = lexicalItem({
    rank: 1,
    matchLine: 12,
    contextStart: 10,
    contextEnd: 14,
    containerId: "function:prepare",
  });
  const result = compactRgContextItems({
    items: [duplicate, { ...duplicate, rank: 2 }],
    rawOccurrences: 2,
  });

  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].occurrences, undefined);
  assert.equal(result.diagnostics.uniqueOccurrences, 1);
  assert.equal(result.diagnostics.exactDuplicatesRemoved, 1);
  assert.equal(result.diagnostics.occurrencesCollapsed, 0);
});

test("applies limit after grouping and reranks logical results", () => {
  const result = compactRgContextItems({
    items: [
      lexicalItem({
        rank: 1,
        matchLine: 10,
        containerId: "function:first",
      }),
      lexicalItem({
        rank: 2,
        matchLine: 11,
        containerId: "function:first",
      }),
      lexicalItem({
        rank: 3,
        matchLine: 20,
        containerId: "function:second",
      }),
      lexicalItem({
        rank: 4,
        matchLine: 30,
        containerId: "function:third",
      }),
    ],
    limit: 2,
  });

  assert.deepEqual(
    result.items.map((item) => [item.rank, item.container.entityId]),
    [
      [1, "function:first"],
      [2, "function:second"],
    ],
  );
  assert.equal(result.diagnostics.groupsFound, 3);
  assert.equal(result.diagnostics.groupsReturned, 2);
  assert.equal(result.diagnostics.groupTruncated, true);
});

test("uses input rank for stable group order", () => {
  const result = compactRgContextItems({
    items: [
      lexicalItem({
        rank: 3,
        matchLine: 30,
        containerId: "function:third",
      }),
      lexicalItem({
        rank: 1,
        matchLine: 10,
        containerId: "function:first",
      }),
      lexicalItem({
        rank: 2,
        matchLine: 20,
        containerId: "function:second",
      }),
    ],
  });

  assert.deepEqual(
    result.items.map((item) => item.container.entityId),
    ["function:first", "function:second", "function:third"],
  );
});

test("only exact-dedupes non-text ranges even when a container is present", () => {
  const byteItem = (rank, range) => ({
    kind: "lexical_match",
    rank,
    file: {
      absolutePath: "/repo/data.bin",
      relativePath: "data.bin",
      rootPath: "/repo",
    },
    range,
    content: "binary match",
    status: "fresh",
    matchedBy: "lexical",
    container: {
      entityId: "artificial-container",
      range: { kind: "file" },
    },
  });
  const result = compactRgContextItems({
    items: [
      byteItem(1, { kind: "byte", startOffset: 1, endOffset: 2 }),
      byteItem(2, { endOffset: 2, startOffset: 1, kind: "byte" }),
      byteItem(3, { kind: "byte", startOffset: 3, endOffset: 4 }),
    ],
  });

  assert.equal(result.items.length, 2);
  assert.equal(result.diagnostics.exactDuplicatesRemoved, 1);
  assert.equal(
    result.items.every((item) => item.occurrences === undefined),
    true,
  );
});

test("bounds representative occurrence windows for a very noisy symbol", () => {
  const occurrenceCount = 6_000;
  const result = compactRgContextItems({
    items: Array.from({ length: occurrenceCount }, (_, index) =>
      lexicalItem({
        rank: index + 1,
        matchLine: index + 1,
        containerId: "function:noisy",
        containerStart: 1,
        containerEnd: occurrenceCount,
      }),
    ),
    limit: 10,
  });

  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].occurrenceCount, occurrenceCount);
  assert.equal(
    result.items[0].occurrences.length,
    MAX_RG_GROUP_OCCURRENCE_SAMPLES,
  );
  assert.equal(result.items[0].occurrences[0].excerptRange.startLine, 1);
  assert.equal(
    result.items[0].occurrences.at(-1).excerptRange.startLine,
    occurrenceCount,
  );
  assert.ok(JSON.stringify(result.items[0]).length < 10_000);
});
