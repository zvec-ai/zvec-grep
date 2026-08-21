import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import { printError } from "../../dist/cli/errors.js";
import {
  contextWarningLines,
  formatAgentContextResult,
  formatCliContextResult,
  printAgentContextResult,
  printCliContextResult,
  printHumanContextResult,
} from "../../dist/cli/format/context.js";
import { printDebug } from "../../dist/cli/format/debug.js";
import { createIndexProgressReporter } from "../../dist/cli/format/progress.js";
import {
  printIndexPathFilterTip,
  printIndexResult,
  printRemoteEmbeddingAuthorizationStatus,
  printServerIndexInfo,
  printWorkspaceInfo,
} from "../../dist/cli/format/status.js";
import { EngineError } from "../../dist/engine/errors.js";

function progressBarGlyphs() {
  return process.platform !== "win32" && process.env.TERM !== "linux"
    ? { filled: "█", empty: "░" }
    : { filled: "#", empty: "-" };
}

function progressBar(filled, empty = 0) {
  const glyphs = progressBarGlyphs();
  return `${glyphs.filled.repeat(filled)}${glyphs.empty.repeat(empty)}`;
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

async function captureConsole(callback) {
  const logs = [];
  const errors = [];
  const originalLog = console.log;
  const originalError = console.error;
  console.log = (...values) => logs.push(values.join(" "));
  console.error = (...values) => errors.push(values.join(" "));
  try {
    await callback();
  } finally {
    console.log = originalLog;
    console.error = originalError;
  }
  return { logs, errors };
}

function status(overrides = {}) {
  return {
    filesScanned: 4,
    filesStored: 3,
    filesIndexed: 2,
    entitiesIndexed: 7,
    fragmentsTruncated: 3,
    filesPending: 1,
    filesFailed: 1,
    filesAdded: 1,
    filesModified: 1,
    filesDeleted: 1,
    filesUnchanged: 0,
    pendingFiles: [],
    failedFiles: [],
    addedFiles: [],
    modifiedFiles: [],
    deletedFiles: [],
    ...overrides,
  };
}

function workspaceIndex(overrides = {}) {
  return {
    id: "workspace-index-1",
    name: "docs workspace index",
    path: "/tmp/index",
    rootPaths: [
      {
        absolutePath: "/repo",
        recursive: true,
        include: ["src/**"],
        exclude: ["**/*.test.ts"],
        ignoreFiles: [".rgignore"],
      },
    ],
    indexPolicy: "enabled",
    embedding: {
      provider: "qwen",
      model: "text-embedding-v4",
      dimension: 16,
      metric: "cosine",
    },
    indexVersion: 1,
    createdTime: 1,
    updatedTime: 2,
    ...overrides,
  };
}

function contextResult(overrides = {}) {
  const lines = Array.from(
    { length: 16 },
    (_, index) => `line ${index + 10} query ${"x".repeat(140)}`,
  ).join("\n");
  return {
    query: "query",
    root: "/repo",
    source: "index",
    coverage: "ranked_sample",
    workspaceIndex: {
      id: "workspace-index-1",
      name: "docs",
      path: "/tmp/index",
    },
    items: [
      {
        kind: "indexed_entity",
        rank: 2,
        file: { absolutePath: "/repo/src/a.ts", relativePath: "src/a.ts" },
        range: {
          kind: "text",
          startLine: 10,
          endLine: 25,
          startOffset: 0,
          endOffset: lines.length,
        },
        excerptRange: {
          kind: "text",
          startLine: 16,
          endLine: 17,
          startOffset: 20,
          endOffset: 40,
        },
        content: lines,
        outline: "export class Container\n  computeQuery\n  helper\n".repeat(3),
        status: "possibly_stale",
        score: 0.81234,
        matchedBy: "fts+vector",
        metadata: {
          kind: "code",
          symbolType: "function",
          symbolName: "computeQuery",
          scope: "Container",
          nodeType: "function_declaration",
          signature: `function computeQuery(${"argument, ".repeat(20)})`,
          doc: "documentation",
          modifiers: ["exported", "async"],
        },
        entityId: "entity-1",
        trace: {
          recall: [
            {
              path: "fts",
              routeId: "lexical",
              query: "query",
              found: true,
              forced: true,
              rank: 1,
            },
            {
              path: "vector",
              query: "query",
              found: false,
              reason: "not recalled",
            },
          ],
          fusion: { rank: 2, score: 0.8, forced: true },
          ranking: { rank: 1, score: 0.9 },
          final: { returnedByLimit: true, cutoffRank: 5 },
        },
      },
      {
        kind: "lexical_match",
        rank: 1,
        file: { absolutePath: "/repo/README.md", relativePath: "README.md" },
        range: {
          kind: "text",
          startLine: 3,
          endLine: 5,
          startOffset: 0,
          endOffset: 30,
        },
        excerptRange: {
          kind: "text",
          startLine: 4,
          endLine: 4,
          startOffset: 10,
          endOffset: 20,
        },
        content: "before\nquery match\nafter",
        status: "fresh",
        matchedBy: "lexical",
        metadata: {
          kind: "markdown",
          heading: "Usage",
          level: 2,
          scope: "Guide",
        },
        container: {
          entityId: "heading-1",
          range: {
            kind: "text",
            startLine: 1,
            endLine: 8,
            startOffset: 0,
            endOffset: 80,
          },
          metadata: {
            kind: "markdown",
            heading: "Usage",
            level: 2,
            scope: "Guide",
          },
        },
      },
    ],
    relationships: [
      {
        srcId: "compute-query",
        dstId: "heading-1",
        srcLabel: "computeQuery",
        dstLabel: "Usage",
        kind: "CALLS",
        scope: "symbol",
      },
    ],
    diagnostics: {
      index: {
        hitsReturned: 2,
        routes: [
          { id: "lexical", mode: "fts", query: "query" },
          { id: "semantic", mode: "vector", query: "query expanded" },
        ],
      },
      rg: {
        backend: "bundled-rg",
        command: "/path with spaces/rg",
        args: ["--json", "query value"],
        ignoredDirectories: ["node_modules", "dist"],
        missingPaths: ["missing one", "missing-two"],
        searchedPaths: ["src"],
        limit: 10,
        truncated: true,
      },
      structure: {
        source: "structural_extraction",
        fileLimit: 100,
        matchedFiles: 2,
        parsedFiles: 2,
        enrichedFiles: 1,
        enrichedItems: 1,
        skippedFiles: 0,
        truncated: false,
      },
      timings: [
        { name: "search", durationMs: 12, count: 2 },
        { name: "format", durationMs: 1 },
      ],
    },
    ...overrides,
  };
}

test("context formatters render indexed, lexical, metadata, preview, trace, and empty results", async () => {
  const result = contextResult();
  const agentText = formatAgentContextResult(result, {
    preview: "short",
    trace: true,
    color: "never",
  });
  const output = await captureConsole(() => {
    printAgentContextResult(result, {
      preview: "short",
      trace: true,
      color: "never",
    });
    printHumanContextResult(result, {
      human: true,
      preview: "full",
      trace: true,
      color: "always",
    });
  });
  assert.equal(
    output.logs.slice(0, agentText.split("\n").length).join("\n"),
    agentText,
  );
  assert.match(output.logs.join("\n"), /README\.md:1-8/);
  assert.match(output.logs.join("\n"), /computeQuery/);
  assert.match(output.logs.join("\n"), /query "query"/);
  assert.match(agentText, /computeQuery --CALLS--> Usage/);
  assert.match(output.logs.join("\n"), /\x1b\[/);
  assert.deepEqual(contextWarningLines(result), [
    "warning: skipped missing paths: missing one, missing-two",
  ]);

  for (const reason of ["no_matches", "no_searchable_files"]) {
    const empty = contextResult({
      source: "rg",
      coverage: "rg_exhaustive",
      workspaceIndex: undefined,
      items: [],
      diagnostics: {
        emptyReason: reason,
        rg: {
          backend: "rg",
          command: "rg",
          args: [],
          ignoredDirectories: [],
          missingPaths: ["missing"],
          truncated: false,
        },
      },
    });
    const emptyOutput = await captureConsole(() => {
      printAgentContextResult(empty, { color: "never" });
      printHumanContextResult(empty, { human: true, color: "never" });
    });
    assert.ok(emptyOutput.logs.length > 0);
    assert.equal(
      contextWarningLines(empty).length,
      reason === "no_searchable_files" ? 0 : 1,
    );
  }
});

test("indexed agent results preserve global rank and expand every candidate", () => {
  const item = (
    rank,
    relativePath,
    content,
    score,
    selectionReason,
    coverageGroup,
  ) => ({
    kind: "indexed_entity",
    rank,
    file: {
      absolutePath: `/repo/${relativePath}`,
      relativePath,
    },
    range: {
      kind: "text",
      startLine: rank * 10,
      endLine: rank * 10,
      startOffset: 0,
      endOffset: content.length,
    },
    content,
    status: "fresh",
    score,
    matchedBy: rank % 2 === 0 ? "fts" : "fts+vector",
    selectionReason,
    coverageGroup,
    metadata: {
      kind: "code",
      symbolType: "function",
      symbolName: `symbol${rank}`,
      scope: null,
      nodeType: "function_declaration",
      modifiers: [],
    },
  });
  const result = contextResult({
    items: [
      item(3, "src/a.ts", "detail-three-body", 0.7, "global_fill"),
      item(1, "src/a.ts", "detail-one-body", 0.9, "coverage", "Q1"),
      item(6, "src/d.ts", "detail-six-body", 0.2, "global_fill"),
      item(2, "src/b.ts", "detail-two-body", 0.8, "global_fill"),
      item(5, "src/c.ts", "detail-five-body", 0.4, "global_fill"),
      item(4, "src/c.ts", "detail-four-body", 0.6, "global_fill"),
      item(7, "src/e.ts", "detail-seven-body", 0.1),
    ],
  });

  const text = formatAgentContextResult(result, {
    preview: "short",
    color: "never",
  });
  for (let rank = 1; rank < 7; rank += 1) {
    assert.ok(text.indexOf(`#${rank} `) < text.indexOf(`#${rank + 1} `));
  }
  assert.doesNotMatch(text, /additional candidates \(metadata only\):/);
  assert.match(text, /#7 matchedBy=fts\+vector src\/e\.ts:70/);
  assert.match(text, /detail-seven-body/);
  assert.doesNotMatch(text, /score=/);

  const traced = formatAgentContextResult(result, {
    preview: "short",
    trace: true,
    color: "never",
  });
  assert.match(
    traced,
    /#1 \[group_coverage: Q1\] matchedBy=fts\+vector score=0\.9000/,
  );
  assert.match(traced, /#7 matchedBy=fts\+vector score=0\.1000/);
});

test("indexed agent results show query-group coverage and provenance", () => {
  const makeItem = (rank, selectionReason, coverageGroup, queryGroups) => ({
    kind: "indexed_entity",
    rank,
    file: {
      absolutePath: `/repo/src/item${rank}.ts`,
      relativePath: `src/item${rank}.ts`,
    },
    range: {
      kind: "text",
      startLine: rank,
      endLine: rank,
      startOffset: 0,
      endOffset: 10,
    },
    content: `result-${rank}`,
    status: "fresh",
    matchedBy: "fts+vector",
    queryGroups,
    selectionReason,
    coverageGroup,
  });
  const result = contextResult({
    items: [
      makeItem(1, "coverage", "Q1", [
        {
          id: "Q1",
          query: "lifecycle",
          role: "primary",
          rank: 1,
          matchedBy: "fts+vector",
        },
        {
          id: "Q3",
          query: "cleanup",
          role: "supplemental",
          rank: 4,
          matchedBy: "vector",
        },
      ]),
      makeItem(2, "coverage", "Q2", [
        {
          id: "Q2",
          query: "backend",
          role: "primary",
          rank: 1,
          matchedBy: "fts",
        },
      ]),
      makeItem(3, "global_fill", undefined, [
        {
          id: "Q1",
          query: "lifecycle",
          role: "primary",
          rank: 2,
          matchedBy: "vector",
        },
      ]),
    ],
    diagnostics: {
      index: {
        hitsReturned: 3,
        queryGroups: [
          { id: "Q1", query: "lifecycle", role: "primary" },
          { id: "Q2", query: "backend", role: "primary" },
          { id: "Q3", query: "cleanup", role: "supplemental" },
        ],
        routes: [],
      },
    },
  });

  const text = formatAgentContextResult(result, {
    preview: "short",
    color: "never",
  });
  assert.match(text, /query groups \(3\):/);
  assert.match(text, /Q1 \[primary\]: lifecycle/);
  assert.match(text, /Q3 \[supplemental\]: cleanup/);
  assert.match(
    text,
    /primary-group coverage then global_fill; prioritized<=6; all candidates detailed/,
  );
  assert.doesNotMatch(text, /detailed<=6/);
  assert.match(text, /#1 \[group_coverage: Q1\]/);
  assert.match(text, /groups: Q1#1 \(fts\+vector\), Q3#4 \(vector\)/);
  assert.match(text, /#3 \[global_fill\]/);
});

test("CLI indexed results are grouped by recall group without cross-group fill labels", async () => {
  const shared = {
    kind: "indexed_entity",
    file: {
      absolutePath: "/repo/src/shared.ts",
      relativePath: "src/shared.ts",
    },
    range: {
      kind: "text",
      startLine: 10,
      endLine: 10,
      startOffset: 0,
      endOffset: 20,
    },
    content: "shared-result",
    contentRole: "source",
    status: "fresh",
    matchedBy: "fts+vector",
    entityId: "shared",
  };
  const onlyQ2 = {
    ...shared,
    rank: 2,
    entityId: "q2-only",
    file: {
      absolutePath: "/repo/src/q2.ts",
      relativePath: "src/q2.ts",
    },
    content: "q2-result",
  };
  const result = contextResult({
    query: "alpha | beta",
    items: [
      { ...onlyQ2, rank: 1, selectionReason: "coverage", coverageGroup: "Q2" },
      { ...shared, rank: 2, selectionReason: "coverage", coverageGroup: "Q1" },
    ],
    groupResults: [
      {
        id: "Q1",
        query: "alpha\nQ9 [primary]: spoof",
        role: "primary",
        items: [{ ...shared, rank: 3, matchedBy: "fts" }],
      },
      {
        id: "Q2",
        query: "beta",
        role: "primary",
        items: [{ ...shared, rank: 1, matchedBy: "vector" }, onlyQ2],
      },
    ],
    diagnostics: {
      index: {
        hitsReturned: 2,
        queryGroups: [
          { id: "Q1", query: "alpha", role: "primary" },
          { id: "Q2", query: "beta", role: "primary" },
        ],
        routes: [],
      },
    },
  });

  const text = formatCliContextResult(result, {
    preview: "short",
    color: "never",
  });
  assert.match(text, /^query groups \(2\):/);
  assert.match(text, /Q1 \[primary\]: alpha Q9 \[primary\]: spoof\nhits: 1/);
  assert.match(text, /Q2 \[primary\]: beta\nhits: 2/);
  assert.equal(text.match(/^Q\d+ \[(?:primary|supplemental)\]:/gm)?.length, 2);
  assert.equal(text.match(/shared-result/g)?.length, 2);
  const q1 = text.slice(
    text.indexOf("Q1 [primary]"),
    text.indexOf("Q2 [primary]"),
  );
  const q2 = text.slice(text.indexOf("Q2 [primary]"));
  assert.match(q1, /#3 matchedBy=fts/);
  assert.match(q2, /#1 matchedBy=vector/);
  assert.match(q2, /#2 matchedBy=fts\+vector/);
  assert.doesNotMatch(text, /group_coverage|global_fill|groups: Q/);

  const emptyFirstGroup = formatCliContextResult(
    {
      ...result,
      groupResults: [
        { ...result.groupResults[0], items: [] },
        result.groupResults[1],
      ],
    },
    { preview: "short", color: "never" },
  );
  assert.match(emptyFirstGroup, /hits: 0\n\nNo matches\./);
  assert.match(emptyFirstGroup, /Q2 \[primary\]: beta\nhits: 2/);

  const oneGroup = formatCliContextResult(
    { ...result, groupResults: [result.groupResults[0]] },
    { preview: "short", color: "never" },
  );
  assert.match(oneGroup, /^query groups \(1\):/);

  const output = await captureConsole(() =>
    printCliContextResult(result, { human: true, color: "never" }),
  );
  assert.match(output.logs.join("\n"), /Group:\s+Q1 \[primary\]/);
  assert.match(output.logs.join("\n"), /Group:\s+Q2 \[primary\]/);
});

test("managed rg uses a compact file and adaptive symbol hierarchy", () => {
  const file = {
    absolutePath: "/repo/src/example.ts",
    relativePath: "src/example.ts",
  };
  const answerMetadata = {
    kind: "code",
    symbolType: "function",
    symbolName: "answer",
    scope: "Widget",
    nodeType: "method_definition",
    signature: null,
    doc: null,
    modifiers: [],
  };
  const result = contextResult({
    source: "rg",
    coverage: "rg_exhaustive",
    workspaceIndex: undefined,
    items: [
      {
        kind: "lexical_match",
        rank: 1,
        file,
        range: {
          kind: "text",
          startLine: 12,
          endLine: 12,
          startOffset: 0,
          endOffset: 6,
        },
        content: "const needle = 42;",
        status: "fresh",
        matchedBy: "lexical",
        container: {
          entityId: "answer",
          range: {
            kind: "text",
            startLine: 10,
            endLine: 20,
            startOffset: 0,
            endOffset: 1,
          },
          metadata: answerMetadata,
        },
      },
      {
        kind: "lexical_match",
        rank: 2,
        file,
        range: {
          kind: "text",
          startLine: 15,
          endLine: 15,
          startOffset: 7,
          endOffset: 13,
        },
        content: "return needle;",
        status: "fresh",
        matchedBy: "lexical",
        container: {
          entityId: "answer",
          range: {
            kind: "text",
            startLine: 10,
            endLine: 20,
            startOffset: 0,
            endOffset: 1,
          },
          metadata: answerMetadata,
        },
      },
      {
        kind: "lexical_match",
        rank: 3,
        file,
        range: {
          kind: "text",
          startLine: 30,
          endLine: 30,
          startOffset: 0,
          endOffset: 6,
        },
        content: "const needle = other();",
        status: "fresh",
        matchedBy: "lexical",
        container: {
          entityId: "other",
          range: {
            kind: "text",
            startLine: 28,
            endLine: 35,
            startOffset: 0,
            endOffset: 1,
          },
          metadata: {
            ...answerMetadata,
            symbolName: "other",
          },
        },
      },
      {
        kind: "lexical_match",
        rank: 4,
        file,
        range: {
          kind: "text",
          startLine: 40,
          endLine: 40,
          startOffset: 6,
          endOffset: 12,
        },
        content: "const moduleNeedle = true;",
        status: "fresh",
        matchedBy: "lexical",
      },
      {
        kind: "lexical_match",
        rank: 5,
        file,
        range: {
          kind: "text",
          startLine: 50,
          endLine: 50,
          startOffset: 9,
          endOffset: 16,
        },
        content: "function declaredNeedle() {}",
        status: "fresh",
        matchedBy: "lexical",
        container: {
          entityId: "declaredNeedle",
          range: {
            kind: "text",
            startLine: 50,
            endLine: 50,
            startOffset: 0,
            endOffset: 28,
          },
          metadata: {
            ...answerMetadata,
            symbolName: "declaredNeedle",
            scope: null,
          },
        },
      },
    ],
    diagnostics: {
      rg: {
        backend: "bundled-rg",
        command: "rg",
        args: [],
        ignoredDirectories: [],
        truncated: false,
      },
    },
  });

  assert.equal(
    formatAgentContextResult(result, { color: "never" }),
    [
      "src/example.ts",
      "  10-20 [function Widget.answer]",
      "    12:\tconst needle = 42;",
      "    15:\treturn needle;",
      "  28-35 [function Widget.other] 30:\tconst needle = other();",
      "  40:\tconst moduleNeedle = true;",
      "  50:\tfunction declaredNeedle() {}",
    ].join("\n"),
  );
});

test("debug formatter reports every diagnostic and trace availability state", async () => {
  const full = await captureConsole(() =>
    printDebug(contextResult(), { trace: true }),
  );
  const text = full.errors.join("\n");
  assert.match(text, /workspace_index=docs/);
  assert.match(text, /rg_command=/);
  assert.match(text, /structural_enrichment=full/);
  assert.match(text, /timings=search:12ms\(2x\)/);
  assert.match(text, /trace=inline/);

  const rg = await captureConsole(() =>
    printDebug(
      contextResult({ source: "rg", workspaceIndex: undefined, items: [] }),
      { trace: true },
    ),
  );
  assert.match(rg.errors.join("\n"), /trace=unavailable source=rg/);

  const noTrace = await captureConsole(() =>
    printDebug(contextResult({ items: [{ ...contextResult().items[1] }] }), {
      trace: true,
    }),
  );
  assert.match(noTrace.errors.join("\n"), /reason=no-hit-trace/);
});

test("status formatters cover workspace states, failures, filters, and color", async () => {
  const failedFile = {
    id: "failed",
    absolutePath: "/repo/fail.ts",
    relativePath: "fail.ts",
    rootPath: "/repo",
    sizeBytes: 1,
    lastModifiedTime: 1,
    kind: "code",
    format: "typescript",
    indexStatus: {
      indexedTime: null,
      entityCount: 0,
      error:
        "ZVEC_GREP.ENGINE.INDEXING.FILE_FAILED: Indexing file failed (stage=embedding file=fail.ts)",
    },
  };
  const stale = status({ failedFiles: [failedFile] });
  const info = workspaceIndex();
  const output = await captureConsole(() => {
    printWorkspaceInfo(
      {
        root: "/repo",
        indexed: true,
        indexPolicy: "enabled",
        source: "index",
        home: "/repo/.zvec-grep",
        indexPath: "/repo/.zvec-grep/index.zvec",
        workspaceIndex: info,
        status: stale,
        suggestion: "zg index",
      },
      { color: "never" },
    );
    printIndexResult(
      "Indexed",
      {
        filesScanned: 4,
        filesAdded: 1,
        filesModified: 1,
        filesPending: 1,
        filesDeleted: 1,
        filesUnchanged: 0,
        filesFailed: 1,
        entitiesCreated: 7,
        durationMs: 65_000,
      },
      {
        color: "never",
        modifiedAfter: 1,
        modifiedBefore: 2,
      },
      info.rootPaths,
    );
    printIndexResult(
      "Quick",
      {
        filesScanned: 0,
        filesAdded: 0,
        filesModified: 0,
        filesPending: 0,
        filesDeleted: 0,
        filesUnchanged: 0,
        filesFailed: 0,
        entitiesCreated: 0,
        durationMs: 250,
      },
      { color: "never" },
    );
    printIndexPathFilterTip({ color: "never" });
    printIndexPathFilterTip({ color: "never", includePaths: [] });
    printServerIndexInfo(
      {
        root: "/repo",
        indexed: true,
        index_policy: "enabled",
        source: "index",
        persistent: {
          home: "/repo/.zvec-grep",
          index_path: "/repo/.zvec-grep/index.zvec",
          workspace_index: {
            root_paths: [
              {
                absolute_path: "/repo",
                recursive: true,
                globs: ["src/**"],
                file_types: ["ts"],
              },
            ],
            embedding: {
              provider: "qwen",
              model: "text-embedding-v4",
              dimension: 16,
              metric: "cosine",
            },
          },
          files: {
            stored: 4,
            scanned: 4,
            indexed: 3,
            pending: 1,
            failed: 0,
            added: 1,
            modified: 0,
            deleted: 0,
            unchanged: 2,
            entities: 7,
            truncated_fragments: 4,
          },
          suggestion: "zg index",
        },
        runtime: {
          job_state: "running",
          progress: { files_indexed: 3, files_total: 4 },
          completion: { completed: 3, total: 4 },
        },
      },
      { color: "never" },
    );
    printServerIndexInfo(
      {
        root: "/failed-repo",
        indexed: false,
        index_policy: "undecided",
        source: "unindexed",
        persistent: {
          home: "/failed-repo/.zvec-grep",
          index_path: "/failed-repo/.zvec-grep/index.zvec",
        },
        runtime: {
          job_state: "failed",
          error: {
            code: "MODEL_LOAD_FAILED",
            message: "Embedding schema could not be resolved.",
          },
        },
      },
      { color: "never" },
    );
  });
  assert.match(output.logs.join("\n"), /Problem\s+fail\.ts/);
  assert.match(output.logs.join("\n"), /Truncated\s+4 fragments/);
  assert.match(output.logs.join("\n"), /1m 5s/);
  assert.match(output.logs.join("\n"), /ignore-file=\.rgignore/);
  assert.match(output.logs.join("\n"), /Default indexing skips/);
  assert.match(output.logs.join("\n"), /zg help file-types/);
  assert.match(
    output.logs.join("\n"),
    new RegExp(
      `◐ Workspace index is updating[\\s\\S]*Coverage\\s+${progressBar(15, 5)}\\s+75%\\s+3 / 4 files`,
    ),
  );
  assert.match(output.logs.join("\n"), /Error\s+MODEL_LOAD_FAILED/);
  assert.match(
    output.logs.join("\n"),
    /Embedding schema could not be resolved/,
  );

  for (const stateInfo of [
    {
      indexPolicy: "disabled",
      indexed: false,
      workspaceIndex: undefined,
      status: null,
    },
    {
      indexPolicy: "undecided",
      indexed: false,
      workspaceIndex: undefined,
      status: null,
    },
    {
      indexPolicy: "enabled",
      indexed: false,
      workspaceIndex: info,
      status: null,
    },
    {
      indexPolicy: "enabled",
      indexed: true,
      workspaceIndex: info,
      status: stale,
    },
    {
      indexPolicy: "enabled",
      indexed: true,
      workspaceIndex: info,
      status: status({
        filesPending: 0,
        filesFailed: 0,
        filesAdded: 0,
        filesModified: 0,
        filesDeleted: 0,
      }),
    },
  ]) {
    const workspace = {
      root: "/repo",
      indexed: stateInfo.indexed,
      indexPolicy: stateInfo.indexPolicy,
      source: stateInfo.indexed ? "index" : "unindexed",
      home: "/repo/.zvec-grep",
      indexPath: "/repo/.zvec-grep/index.zvec",
      workspaceIndex: stateInfo.workspaceIndex,
      status: stateInfo.status,
      suggestion: "zg index",
    };
    const rendered = await captureConsole(() =>
      printWorkspaceInfo(workspace, { color: "never" }),
    );
    assert.match(rendered.logs.join("\n"), /Workspace index/);
  }
});

test("index formatter emits skipped-file details only in debug mode", async () => {
  const result = {
    filesScanned: 1,
    filesAdded: 1,
    filesModified: 0,
    filesPending: 0,
    filesDeleted: 0,
    filesUnchanged: 0,
    filesFailed: 0,
    entitiesCreated: 1,
    durationMs: 10,
    scanDiagnostics: {
      skippedFiles: 1,
      skippedByReason: {
        empty: 0,
        too_large: 1,
        unsupported: 0,
        binary: 0,
      },
      skippedSamples: [
        {
          absolutePath: "/repo/large.ts",
          relativePath: "large.ts",
          reason: "too_large",
          sizeBytes: 2 * 1024 * 1024,
          limitBytes: 1024 * 1024,
        },
      ],
    },
  };

  const normal = await captureConsole(() =>
    printIndexResult("Indexed", result, { color: "never" }),
  );
  assert.equal(normal.errors.length, 0);

  const debug = await captureConsole(() =>
    printIndexResult("Indexed", result, { color: "never", debug: true }),
  );
  assert.match(debug.errors.join("\n"), /skipped_files=1/);
  assert.match(debug.errors.join("\n"), /path="large\.ts"/);
  assert.match(debug.errors.join("\n"), /limit_bytes=1048576/);
});

test("workspace status uses a status-first grouped layout", async () => {
  const originalTerm = process.env.TERM;
  process.env.TERM = "xterm-256color";
  try {
    const output = await captureConsole(() =>
      printWorkspaceInfo(
        {
          root: "/repo",
          indexed: true,
          indexPolicy: "enabled",
          source: "index",
          home: "/repo/.zvec-grep",
          indexPath: "/repo/.zvec-grep/index.zvec",
          workspaceIndex: workspaceIndex({
            rootPaths: [{ absolutePath: "/repo", recursive: true }],
          }),
          status: status({
            filesScanned: 1132,
            filesStored: 1132,
            filesIndexed: 1132,
            entitiesIndexed: 22037,
            fragmentsTruncated: 12,
            filesPending: 0,
            filesFailed: 0,
            filesAdded: 0,
            filesModified: 0,
            filesDeleted: 0,
            filesUnchanged: 1132,
          }),
        },
        { color: "never" },
      ),
    );

    assert.deepEqual(output.logs, [
      "✓ Workspace index is ready",
      "  /repo",
      "",
      `  Coverage    ${progressBar(20)} 100%  1,132 / 1,132 files`,
      "  Entities    22,037",
      "  Truncated   12 fragments",
      "  Queue       0 pending · 0 failed",
      "",
      "  Embedding   qwen/text-embedding-v4",
      "              16 dimensions · cosine",
      "",
      `  Storage     ${path.join(".zvec-grep", "index.zvec")}`,
    ]);

    const colored = await captureConsole(() =>
      printServerIndexInfo(
        {
          root: "/repo",
          indexed: true,
          index_policy: "enabled",
          source: "index",
          persistent: {
            home: "/repo/.zvec-grep",
            index_path: "/repo/.zvec-grep/index.zvec",
            workspace_index: {
              root_paths: [{ absolute_path: "/repo", recursive: true }],
              embedding: {
                provider: "qwen",
                model: "text-embedding-v4",
                dimension: 1024,
                metric: "cosine",
              },
            },
            files: {
              stored: 2,
              indexed: 1,
              pending: 1,
              failed: 1,
              added: 0,
              modified: 0,
              deleted: 0,
              unchanged: 1,
              entities: 3,
              truncated_fragments: 0,
            },
          },
        },
        { color: "always" },
      ),
    );
    const text = colored.logs.join("\n");
    assert.match(text, /\x1b\[31m✗ Workspace index failed/);
    assert.match(
      text,
      new RegExp(
        `\\x1b\\[38;2;22;163;74m${escapeRegExp(progressBarGlyphs().filled)}`,
      ),
    );
    assert.match(text, /\x1b\[33m1 pending/);
    assert.match(text, /\x1b\[31m1 failed/);
    assert.doesNotMatch(text, /policy|indexed\s+yes|source\s+index|home/);
  } finally {
    if (originalTerm === undefined) delete process.env.TERM;
    else process.env.TERM = originalTerm;
  }
});

test("server workspace status reports changed files as stale", async () => {
  const output = await captureConsole(() =>
    printServerIndexInfo(
      {
        root: "/repo",
        indexed: true,
        index_policy: "enabled",
        source: "index",
        persistent: {
          home: "/repo/.zvec-grep",
          index_path: "/repo/.zvec-grep/index.zvec",
          workspace_index: {
            root_paths: [{ absolute_path: "/repo", recursive: true }],
            embedding: {
              provider: "qwen",
              model: "text-embedding-v4",
              dimension: 16,
              metric: "cosine",
            },
          },
          files: {
            stored: 8,
            scanned: 6,
            indexed: 8,
            pending: 0,
            failed: 0,
            added: 1,
            modified: 2,
            deleted: 3,
            unchanged: 3,
            entities: 7,
            truncated_fragments: 0,
          },
        },
      },
      { color: "never" },
    ),
  );

  assert.match(output.logs.join("\n"), /Workspace index needs an update/);
  assert.match(
    output.logs.join("\n"),
    new RegExp(`Coverage\\s+${progressBar(10, 10)}\\s+50%\\s+3 / 6 files`),
  );
  assert.match(
    output.logs.join("\n"),
    /Changes\s+1 added · 2 modified · 3 deleted/,
  );
});

test("direct workspace status excludes changed files from coverage", async () => {
  const output = await captureConsole(() =>
    printWorkspaceInfo(
      {
        root: "/repo",
        indexed: true,
        indexPolicy: "enabled",
        source: "index",
        home: "/repo/.zvec-grep",
        indexPath: "/repo/.zvec-grep/index.zvec",
        workspaceIndex: workspaceIndex({
          rootPaths: [{ absolutePath: "/repo", recursive: true }],
        }),
        status: status({
          filesScanned: 173,
          filesStored: 193,
          filesIndexed: 193,
          filesPending: 0,
          filesFailed: 0,
          filesAdded: 0,
          filesModified: 1,
          filesDeleted: 20,
          filesUnchanged: 172,
        }),
      },
      { color: "never" },
    ),
  );

  assert.match(output.logs.join("\n"), /Workspace index needs an update/);
  assert.match(
    output.logs.join("\n"),
    new RegExp(`Coverage\\s+${progressBar(20)}\\s+99%\\s+172 / 173 files`),
  );
  assert.doesNotMatch(output.logs.join("\n"), /100%\s+173 \/ 173 files/);
});

test("server updating coverage uses the full configured scope", async () => {
  const output = await captureConsole(() =>
    printServerIndexInfo(
      {
        root: "/repo",
        indexed: true,
        index_policy: "enabled",
        source: "index",
        persistent: {
          home: "/repo/.zvec-grep",
          index_path: "/repo/.zvec-grep/index.zvec",
          files: {
            stored: 1011,
            scanned: 1000,
            indexed: 1000,
            pending: 0,
            failed: 0,
            added: 9,
            modified: 104,
            deleted: 20,
            unchanged: 887,
            entities: 22138,
          },
        },
        runtime: {
          job_state: "running",
          progress: {
            files_indexed: 89,
            files_total: 113,
          },
          completion: {
            completed: 976,
            total: 1000,
          },
        },
      },
      { color: "never" },
    ),
  );

  const text = output.logs.join("\n");
  assert.match(
    text,
    new RegExp(`Coverage\\s+${progressBar(20)}\\s+98%\\s+976 / 1,000 files`),
  );
  assert.doesNotMatch(text, /89 \/ 113 files/);
});

test("Remote Embedding authorization status uses grouped colors and compact paths", async () => {
  const status = {
    path: "/repo/.zvec-grep/authorization.json",
    grants: [
      {
        version: 1,
        id: "grant-1",
        capability: "remote_embedding",
        scope: "workspace",
        workspaceRoots: ["/repo"],
        workspaceFingerprint: "workspace",
        provider: "qwen",
        model: "text-embedding-v4",
        endpoint:
          "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings",
        targetFingerprint: "target",
        grantedAt: 1,
        valid: true,
      },
    ],
  };
  const output = await captureConsole(() =>
    printRemoteEmbeddingAuthorizationStatus("/repo", status, {
      color: "never",
    }),
  );
  assert.deepEqual(output.logs, [
    "✓ Remote Embedding is authorized",
    "  /repo",
    "",
    "Authorization",
    "  Scope       Workspace",
    "  Target      qwen/text-embedding-v4",
    "  Endpoint    dashscope.aliyuncs.com",
    "",
    "Storage",
    `  Grant       ${path.join(".zvec-grep", "authorization.json")}`,
  ]);

  const colored = await captureConsole(() =>
    printRemoteEmbeddingAuthorizationStatus("/repo", status, {
      color: "always",
    }),
  );
  const coloredText = colored.logs.join("\n");
  assert.match(coloredText, /\x1b\[32m✓ Remote Embedding is authorized/);
  assert.match(coloredText, /\x1b\[32mWorkspace/);
  assert.match(coloredText, /\x1b\[36m\/repo/);
  assert.match(
    coloredText,
    new RegExp(
      `\\x1b\\[36m${escapeRegExp(path.join(".zvec-grep", "authorization.json"))}`,
    ),
  );

  const missing = await captureConsole(() =>
    printRemoteEmbeddingAuthorizationStatus(
      "/repo",
      { path: status.path, grants: [] },
      { color: "never" },
    ),
  );
  assert.deepEqual(missing.logs, [
    "○ Remote Embedding is not authorized",
    "  /repo",
    "",
    "Run",
    "  zg auth grant --capability embedding --scope workspace",
  ]);
});

test("error formatter renders engine context, causes, plain values, and color", async () => {
  const engineError = new EngineError("Index failed", {
    code: "ZVEC_GREP.ENGINE.TEST.FAILURE",
    context:
      "workspaceIndex=workspace file=src/a.ts\nstage=embedding\nplain detail",
    cause: new Error("network unavailable"),
  });
  const output = await captureConsole(() => {
    printError(engineError, { color: "always" });
    printError(new Error("plain error", { cause: "string cause" }), {
      color: "never",
    });
    printError("plain value", { color: "never" });
  });
  const text = output.errors.join("\n");
  assert.match(text, /Code:/);
  assert.match(text, /file: src\/a\.ts/);
  assert.match(text, /workspaceIndex: workspace/);
  assert.match(text, /network unavailable/);
  assert.match(text, /string cause/);
  assert.match(text, /plain value/);
  assert.match(text, /\x1b\[/);
});

test("progress reporter covers TTY and non-TTY phases, counters, truncation, and idempotent finish", () => {
  const originalWrite = process.stderr.write;
  const ttyDescriptor = Object.getOwnPropertyDescriptor(
    process.stderr,
    "isTTY",
  );
  const columnsDescriptor = Object.getOwnPropertyDescriptor(
    process.stderr,
    "columns",
  );
  const originalTerm = process.env.TERM;
  const writes = [];
  let ttyStart;
  let ttyProgressGlyphs;
  process.stderr.write = (value) => {
    writes.push(String(value));
    return true;
  };
  try {
    Object.defineProperty(process.stderr, "isTTY", {
      configurable: true,
      value: false,
    });
    const nonTty = createIndexProgressReporter({ nonTtyIntervalMs: 0 });
    nonTty.report({ phase: "scanning" });
    nonTty.report({ phase: "scanning", detail: "Scanning workspace" });
    nonTty.report({ phase: "indexing" });
    nonTty.report({
      phase: "indexing",
      embedding: {
        stage: "preparing",
        model: "local/test-download",
      },
    });
    nonTty.report({
      phase: "indexing",
      filesIndexed: 0,
      filesTotal: 5,
      embedding: {
        stage: "downloading",
        model: "local/test-download",
        downloadedBytes: 25,
        totalBytes: 100,
      },
    });
    nonTty.report({
      phase: "indexing",
      filesIndexed: 0,
      filesTotal: 5,
      detail: "queued indexing progress",
    });
    nonTty.report({
      phase: "indexing",
      embedding: {
        stage: "warning",
        model: "local/test-download",
        message:
          "GPU initialization failed,\n\x1b[31mforged warning\rfalling back to CPU.",
      },
    });
    nonTty.report({
      phase: "indexing",
      embedding: {
        stage: "ready",
        model: "local/test-download",
      },
    });
    nonTty.report({
      phase: "indexing",
      filesIndexed: 2,
      filesTotal: 5,
      filesFailed: 1,
      detail: "x".repeat(150),
      embedding: { concurrency: 3, retryableFailures: 2 },
    });
    nonTty.report({ phase: "indexing", embedding: {} });
    nonTty.report({ phase: "done" });
    nonTty.report({ phase: "done", detail: "Custom completion" });
    nonTty.reportLine("Server index progress");
    nonTty.finish();
    nonTty.finish();

    ttyStart = writes.length;
    process.env.TERM = "xterm-256color";
    ttyProgressGlyphs = progressBarGlyphs();
    Object.defineProperty(process.stderr, "isTTY", {
      configurable: true,
      value: true,
    });
    Object.defineProperty(process.stderr, "columns", {
      configurable: true,
      value: 100,
    });
    const tty = createIndexProgressReporter({ color: "always" });
    tty.report({
      phase: "indexing",
      embedding: {
        stage: "preparing",
        model: "local/test-download",
      },
    });
    tty.report({
      phase: "indexing",
      filesIndexed: 0,
      filesTotal: 10,
      embedding: {
        stage: "downloading",
        model: "local/test-download",
        downloadedBytes: 25,
        totalBytes: 100,
      },
    });
    tty.report({
      phase: "indexing",
      filesIndexed: 0,
      filesTotal: 10,
      detail: "queued indexing progress",
    });
    tty.report({
      phase: "indexing",
      embedding: {
        stage: "warning",
        model: "local/test-download",
        message:
          "GPU initialization failed,\n\x1b[31mforged warning\rfalling back to CPU.",
      },
    });
    tty.report({
      phase: "indexing",
      embedding: {
        stage: "ready",
        model: "local/test-download",
      },
    });
    tty.report({
      phase: "indexing",
      filesIndexed: 1,
      filesTotal: 10,
      detail: "a long progress detail",
      embedding: { concurrency: 3 },
    });
    tty.report({ phase: "done", detail: "done" });
    tty.finish();
  } finally {
    process.stderr.write = originalWrite;
    if (ttyDescriptor) {
      Object.defineProperty(process.stderr, "isTTY", ttyDescriptor);
    } else {
      delete process.stderr.isTTY;
    }
    if (columnsDescriptor) {
      Object.defineProperty(process.stderr, "columns", columnsDescriptor);
    } else {
      delete process.stderr.columns;
    }
    if (originalTerm === undefined) {
      delete process.env.TERM;
    } else {
      process.env.TERM = originalTerm;
    }
  }

  const text = writes.join("");
  const ttyText = writes.slice(ttyStart).join("");
  assert.match(text, /Scanning workspace/);
  assert.match(text, /Preparing local\/test-download/);
  assert.match(text, /Downloading local\/test-download · 25% · 25 B\/100 B/);
  assert.match(ttyText, /Downloading local\/test-download · 25% · 25 B\/100 B/);
  assert.match(
    text,
    /zvec-grep warning: GPU initialization failed, forged warning falling back to CPU\.\n/,
  );
  assert.doesNotMatch(text, /\x1b\[31mforged warning/);
  assert.ok(
    text.indexOf("Downloading local/test-download") <
      text.indexOf("zvec-grep warning: GPU initialization failed") &&
      text.indexOf("zvec-grep warning: GPU initialization failed") <
        text.indexOf("queued indexing progress"),
  );
  assert.ok(
    ttyText.indexOf("Downloading local/test-download") <
      ttyText.indexOf("zvec-grep warning: GPU initialization failed") &&
      ttyText.indexOf("zvec-grep warning: GPU initialization failed") <
        ttyText.indexOf("0%  0/10"),
  );
  assert.match(text, /failed=1/);
  assert.match(text, /concurrency=3 retries=2/);
  assert.match(text, /Indexing complete/);
  assert.match(text, /Server index progress/);
  assert.match(ttyText, /\x1b\[\?25l/);
  assert.match(ttyText, /\r\x1b\[2K/);
  assert.match(
    ttyText,
    new RegExp(
      `\\x1b\\[38;2;\\d+;\\d+;\\d+m${escapeRegExp(ttyProgressGlyphs.filled)}`,
    ),
  );
  assert.match(ttyText, new RegExp(escapeRegExp(ttyProgressGlyphs.empty)));
  assert.match(ttyText, /10%\s+1\/10\s+3 workers/);
  assert.match(ttyText, /100%\s+10\/10\s+3 workers/);
  assert.doesNotMatch(ttyText, /a long progress detail/);
  assert.match(ttyText, /\n\x1b\[\?25h$/);
});
