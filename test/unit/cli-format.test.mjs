import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import { printError } from "../../dist/cli/errors.js";
import {
  contextWarningLines,
  formatAgentContextResult,
  printAgentContextResult,
  printHumanContextResult,
} from "../../dist/cli/format/context.js";
import { printDebug } from "../../dist/cli/format/debug.js";
import { createIndexProgressReporter } from "../../dist/cli/format/progress.js";
import {
  printAnonymousInfo,
  printCollectionInfo,
  printCollectionList,
  printCollectionRemoveResult,
  printIndexPathFilterTip,
  printIndexResult,
  printRemoteEmbeddingAuthorizationStatus,
  printServerIndexInfo,
} from "../../dist/cli/format/status.js";
import { EngineError } from "../../dist/engine/errors/index.js";

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
    collectionId: "collection-1",
    collectionName: "docs",
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

function collection(overrides = {}) {
  return {
    id: "collection-1",
    name: "docs collection",
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
    collection: {
      id: "collection-1",
      name: "docs",
      path: "/tmp/index",
      anonymous: false,
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
  assert.match(output.logs.join("\n"), /\x1b\[/);
  assert.deepEqual(contextWarningLines(result), [
    "warning: skipped missing paths: missing one, missing-two",
  ]);

  for (const reason of ["no_matches", "no_searchable_files"]) {
    const empty = contextResult({
      source: "rg",
      coverage: "rg_exhaustive",
      collection: undefined,
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

test("context formatters summarize grouped symbols without merging occurrence windows", async () => {
  const item = {
    kind: "lexical_match",
    rank: 1,
    file: { absolutePath: "/repo/src/a.ts", relativePath: "src/a.ts" },
    range: {
      kind: "text",
      startLine: 10,
      endLine: 12,
      startOffset: 0,
      endOffset: 9,
    },
    excerptRange: {
      kind: "text",
      startLine: 11,
      endLine: 11,
      startOffset: 0,
      endOffset: 9,
    },
    content: "line 10\nquery first\nshared 12",
    occurrences: [
      {
        rank: 1,
        range: {
          kind: "text",
          startLine: 10,
          endLine: 12,
          startOffset: 0,
          endOffset: 9,
        },
        excerptRange: {
          kind: "text",
          startLine: 11,
          endLine: 11,
          startOffset: 0,
          endOffset: 9,
        },
      },
      {
        rank: 2,
        range: {
          kind: "text",
          startLine: 12,
          endLine: 14,
          startOffset: 0,
          endOffset: 9,
        },
        excerptRange: {
          kind: "text",
          startLine: 13,
          endLine: 13,
          startOffset: 0,
          endOffset: 9,
        },
      },
      {
        rank: 3,
        range: {
          kind: "text",
          startLine: 20,
          endLine: 21,
          startOffset: 0,
          endOffset: 9,
        },
        excerptRange: {
          kind: "text",
          startLine: 20,
          endLine: 20,
          startOffset: 0,
          endOffset: 9,
        },
      },
    ],
    status: "fresh",
    matchedBy: "lexical",
    metadata: {
      kind: "code",
      symbolType: "function",
      symbolName: "prepare",
      scope: "Container",
      nodeType: "function_declaration",
      signature: "function prepare()",
      doc: null,
      modifiers: [],
    },
    container: {
      entityId: "prepare",
      range: {
        kind: "text",
        startLine: 8,
        endLine: 25,
        startOffset: 0,
        endOffset: 9,
      },
      metadata: {
        kind: "code",
        symbolType: "function",
        symbolName: "prepare",
        scope: "Container",
        nodeType: "function_declaration",
        signature: "function prepare()",
        doc: null,
        modifiers: [],
      },
    },
  };
  const result = contextResult({
    query: "query",
    source: "rg",
    coverage: "rg_exhaustive",
    collection: undefined,
    items: [item],
    diagnostics: {
      rg: {
        backend: "rg",
        command: "rg",
        args: ["--json", "query"],
        ignoredDirectories: [],
        truncated: false,
      },
    },
  });

  const agentText = formatAgentContextResult(result, { color: "never" });
  assert.equal(
    agentText,
    [
      "src/a.ts:8-25",
      "symbol: function prepare scope: Container",
      "matches: 3 at L11, L13, L20",
      "10-\tline 10",
      "11:\tquery first",
      "12-\tshared 12",
    ].join("\n"),
  );
  assert.equal((agentText.match(/shared 12/g) ?? []).length, 1);
  assert.doesNotMatch(agentText, /query second|query third/);

  const human = await captureConsole(() =>
    printHumanContextResult(result, { human: true, color: "never" }),
  );
  const humanText = human.logs.join("\n");
  assert.match(humanText, /^Hits: 3$/m);
  assert.match(humanText, /Matches: 3 at L11, L13, L20/);
  assert.equal((humanText.match(/shared 12/g) ?? []).length, 1);
  assert.doesNotMatch(humanText, /query second|query third/);

  const truncatedText = formatAgentContextResult(
    {
      ...result,
      coverage: "rg_truncated",
      diagnostics: {
        rg: {
          ...result.diagnostics.rg,
          truncated: true,
        },
      },
    },
    { color: "never" },
  );
  assert.match(truncatedText, /matches: 3\+ at L11, L13, L20/);
});

test("a single lexical occurrence preserves the existing formatter output", async () => {
  const baseItem = contextResult().items[1];
  const withOccurrence = {
    ...baseItem,
    occurrences: [
      {
        rank: baseItem.rank,
        range: baseItem.range,
        excerptRange: baseItem.excerptRange,
      },
    ],
  };
  const baseResult = contextResult({
    source: "rg",
    coverage: "rg_exhaustive",
    collection: undefined,
    items: [baseItem],
  });
  const occurrenceResult = contextResult({
    source: "rg",
    coverage: "rg_exhaustive",
    collection: undefined,
    items: [withOccurrence],
  });

  assert.equal(
    formatAgentContextResult(occurrenceResult, { color: "never" }),
    formatAgentContextResult(baseResult, { color: "never" }),
  );

  const baseHuman = await captureConsole(() =>
    printHumanContextResult(baseResult, { human: true, color: "never" }),
  );
  const occurrenceHuman = await captureConsole(() =>
    printHumanContextResult(occurrenceResult, {
      human: true,
      color: "never",
    }),
  );
  assert.deepEqual(occurrenceHuman.logs, baseHuman.logs);
});

test("grouped lexical rendering does not expand representative occurrence windows", () => {
  const occurrences = Array.from({ length: 8 }, (_, index) => {
    const startLine = index * 100 + 1;
    return {
      rank: index + 1,
      range: {
        kind: "text",
        startLine,
        endLine: startLine + 40,
        startOffset: 0,
        endOffset: 9,
      },
      excerptRange: {
        kind: "text",
        startLine: startLine + 20,
        endLine: startLine + 20,
        startOffset: 0,
        endOffset: 9,
      },
    };
  });
  const baseContent = Array.from(
    { length: 41 },
    (_, line) => `window 0 line ${line}`,
  ).join("\n");
  const item = {
    kind: "lexical_match",
    rank: 1,
    file: { absolutePath: "/repo/noisy.ts", relativePath: "noisy.ts" },
    range: occurrences[0].range,
    excerptRange: occurrences[0].excerptRange,
    content: baseContent,
    occurrenceCount: 6_000,
    occurrences,
    status: "fresh",
    matchedBy: "lexical",
    container: {
      entityId: "function:noisy",
      range: {
        kind: "text",
        startLine: 1,
        endLine: 741,
        startOffset: 0,
        endOffset: 9,
      },
    },
  };
  const text = formatAgentContextResult(
    contextResult({
      query: "needle",
      source: "rg",
      coverage: "rg_exhaustive",
      collection: undefined,
      items: [item],
    }),
    { color: "never" },
  );

  assert.match(text, /matches: 6000\+? at .*?, \.\.\./);
  assert.equal(
    text.split("\n").filter((line) => /^\d+[-:]\t/.test(line)).length,
    41,
  );
  assert.doesNotMatch(text, /window 1 line/);
});

test("debug formatter reports every diagnostic and trace availability state", async () => {
  const full = await captureConsole(() =>
    printDebug(contextResult(), { trace: true }),
  );
  const text = full.errors.join("\n");
  assert.match(text, /collection=docs/);
  assert.match(text, /rg_command=/);
  assert.match(text, /structural_enrichment=full/);
  assert.match(text, /timings=search:12ms\(2x\)/);
  assert.match(text, /trace=inline/);

  const rg = await captureConsole(() =>
    printDebug(
      contextResult({ source: "rg", collection: undefined, items: [] }),
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

test("status formatters cover collections, anonymous states, failures, filters, and color", async () => {
  const failedFile = {
    id: "failed",
    collectionId: "collection-1",
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
  const info = collection();
  const output = await captureConsole(() => {
    printCollectionList([info], { color: "never" });
    printCollectionInfo(info, stale, { color: "never" });
    printCollectionInfo(collection({ embedding: null }), null, {
      color: "always",
    });
    printIndexResult(
      "Indexed",
      {
        collectionId: "collection-1",
        collectionName: "docs",
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
        collectionId: "collection-1",
        collectionName: "docs",
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
    printCollectionRemoveResult("docs", true, { color: "never" });
    printCollectionRemoveResult("missing", false, { color: "never" });
    printServerIndexInfo(
      {
        root: "/repo",
        indexed: true,
        index_policy: "enabled",
        source: "index",
        persistent: {
          home: "/repo/.zvec-grep",
          index_path: "/repo/.zvec-grep/index",
          collection: {
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
          index_path: "/failed-repo/.zvec-grep/index",
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
  assert.match(output.logs.join("\n"), /failed_reasons/);
  assert.match(output.logs.join("\n"), /Truncated\s+4 fragments/);
  assert.match(output.logs.join("\n"), /1m 5s/);
  assert.match(output.logs.join("\n"), /ignore-file=\.rgignore/);
  assert.match(output.logs.join("\n"), /Default indexing skips/);
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
      collection: undefined,
      status: null,
    },
    {
      indexPolicy: "undecided",
      indexed: false,
      collection: undefined,
      status: null,
    },
    {
      indexPolicy: "enabled",
      indexed: false,
      collection: info,
      status: null,
    },
    {
      indexPolicy: "enabled",
      indexed: true,
      collection: info,
      status: stale,
    },
    {
      indexPolicy: "enabled",
      indexed: true,
      collection: info,
      status: status({
        filesPending: 0,
        filesFailed: 0,
        filesAdded: 0,
        filesModified: 0,
        filesDeleted: 0,
      }),
    },
  ]) {
    const anonymous = {
      root: "/repo",
      indexed: stateInfo.indexed,
      indexPolicy: stateInfo.indexPolicy,
      source: stateInfo.indexed ? "index" : "unindexed",
      home: "/repo/.zvec-grep",
      indexPath: "/repo/.zvec-grep/index",
      collection: stateInfo.collection,
      status: stateInfo.status,
      suggestion: "zg index",
    };
    const rendered = await captureConsole(() =>
      printAnonymousInfo(anonymous, { color: "never" }),
    );
    assert.match(rendered.logs.join("\n"), /Workspace index/);
  }
});

test("workspace status uses a status-first grouped layout", async () => {
  const originalTerm = process.env.TERM;
  process.env.TERM = "xterm-256color";
  try {
    const output = await captureConsole(() =>
      printAnonymousInfo(
        {
          root: "/repo",
          indexed: true,
          indexPolicy: "enabled",
          source: "index",
          home: "/repo/.zvec-grep",
          indexPath: "/repo/.zvec-grep/index",
          collection: collection({
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
      `  Storage     ${path.join(".zvec-grep", "index")}`,
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
            index_path: "/repo/.zvec-grep/index",
            collection: {
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
          index_path: "/repo/.zvec-grep/index",
          collection: {
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
    printAnonymousInfo(
      {
        root: "/repo",
        indexed: true,
        indexPolicy: "enabled",
        source: "index",
        home: "/repo/.zvec-grep",
        indexPath: "/repo/.zvec-grep/index",
        collection: collection({
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
          index_path: "/repo/.zvec-grep/index",
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
      "collection=__anonymous__ file=src/a.ts\nstage=embedding\nplain detail",
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
  assert.doesNotMatch(text, /__anonymous__/);
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
