import assert from "node:assert/strict";
import test from "node:test";
import { printError } from "../../dist/cli/errors.js";
import {
  contextWarningLines,
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
  printServerIndexInfo,
} from "../../dist/cli/format/status.js";
import { EngineError } from "../../dist/engine/errors/index.js";

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
            indexed: 3,
            pending: 1,
            failed: 0,
            entities: 7,
          },
          suggestion: "zg index",
        },
        runtime: {
          job_state: "running",
          progress: { files_indexed: 3, files_total: 4 },
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
  assert.match(output.logs.join("\n"), /1m 5s/);
  assert.match(output.logs.join("\n"), /ignore-file=\.rgignore/);
  assert.match(output.logs.join("\n"), /Default indexing skips/);
  assert.match(
    output.logs.join("\n"),
    /◐ Workspace index is updating[\s\S]*Coverage\s+█{15}░{5}\s+75%\s+3 \/ 4 files/,
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
            filesPending: 0,
            filesFailed: 0,
            filesAdded: 0,
            filesModified: 0,
            filesDeleted: 0,
          }),
        },
        { color: "never" },
      ),
    );

    assert.deepEqual(output.logs, [
      "✓ Workspace index is ready",
      "  /repo",
      "",
      "  Coverage    ████████████████████ 100%  1,132 / 1,132 files",
      "  Entities    22,037",
      "  Queue       0 pending · 0 failed",
      "",
      "  Embedding   qwen/text-embedding-v4",
      "              16 dimensions · cosine",
      "",
      "  Storage     .zvec-grep/index",
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
              entities: 3,
            },
          },
        },
        { color: "always" },
      ),
    );
    const text = colored.logs.join("\n");
    assert.match(text, /\x1b\[31m✗ Workspace index failed/);
    assert.match(text, /\x1b\[38;2;22;163;74m█/);
    assert.match(text, /\x1b\[33m1 pending/);
    assert.match(text, /\x1b\[31m1 failed/);
    assert.doesNotMatch(text, /policy|indexed\s+yes|source\s+index|home/);
  } finally {
    if (originalTerm === undefined) delete process.env.TERM;
    else process.env.TERM = originalTerm;
  }
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
  assert.match(ttyText, /\x1b\[38;2;\d+;\d+;\d+m█/);
  assert.match(ttyText, /░/);
  assert.match(ttyText, /10%\s+1\/10\s+3 workers/);
  assert.match(ttyText, /100%\s+10\/10\s+3 workers/);
  assert.doesNotMatch(ttyText, /a long progress detail/);
  assert.match(ttyText, /\n\x1b\[\?25h$/);
});
