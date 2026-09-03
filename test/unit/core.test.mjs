import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import {
  parseArgs,
  parseDevice,
  parseModifiedTime,
  splitPathFilters,
} from "../../dist/cli/args.js";
import { colorModeFromArgs } from "../../dist/cli/errors.js";
import {
  createHighlighter,
  shouldUseColor,
} from "../../dist/cli/format/highlight.js";
import { rangeLabel, rangeStartLine } from "../../dist/cli/format/range.js";
import {
  detail,
  EngineError,
  errorDetails,
  isEngineError,
} from "../../dist/engine/errors.js";
import { makeEntityId } from "../../dist/engine/extraction/ids.js";
import { detectFileType } from "../../dist/engine/file-type.js";
import {
  getEmbeddingModelCatalogEntry,
  listEmbeddingModels,
} from "../../dist/engine/models/catalog.js";
import { isImageContent, isTextContent } from "../../dist/engine/types.js";
import {
  hasPathGlob,
  isAbsolutePathPattern,
  normalizePathForMatch,
  normalizePathPattern,
  pathPatternMatches,
  pathPatternMatchesCaseInsensitive,
  pathPatternMightMatchDescendant,
  ripgrepGlobMatches,
  ripgrepGlobMatchesCaseInsensitive,
} from "../../dist/engine/utils/glob.js";
import {
  matchesFileSelection,
  resolveFileTypePatterns,
} from "../../dist/engine/utils/file-selection.js";
import {
  isPathInside,
  normalizePath,
  toDisplayPath,
} from "../../dist/engine/utils/path.js";
import {
  ConcurrentTiming,
  TimingCollector,
} from "../../dist/engine/utils/timing.js";
import { contextOptionsFromRgInput } from "../../dist/mcp/input-normalization.js";

test("CLI argument parser handles command, provider, path, and rg options", () => {
  const parsed = parseArgs([
    "query",
    "--human",
    "--trace",
    "--preview",
    "short",
    "--limit",
    "7",
    "-g",
    "src/**",
    "-gtest/**",
    "--glob=!dist/**",
    "--modified-after",
    "2026-01-01",
    "query text",
  ]);
  assert.equal(parsed.command, "query");
  assert.deepEqual(parsed.positionals, ["query text"]);
  assert.equal(parsed.options.human, true);
  assert.equal(parsed.options.trace, true);
  assert.equal(parsed.options.preview, "short");
  assert.equal(parsed.options.limit, 7);
  assert.deepEqual(parsed.options.globs, ["src/**", "test/**", "!dist/**"]);
  assert.equal(typeof parsed.options.modifiedAfter, "number");

  const index = parseArgs([
    "index",
    "--embedding",
    "qwen/text-embedding-v4",
    "--api-key",
    "secret",
    "--endpoint",
    "https://example.test/embeddings",
    "--model-cache",
    "/tmp/models",
    "--device",
    "CUDA",
    "--embedding-concurrency",
    "4",
  ]);
  assert.equal(index.options.embedding, "qwen/text-embedding-v4");
  assert.equal(index.options.endpoint, "https://example.test/embeddings");
  assert.equal(index.options.modelCacheDir, "/tmp/models");
  assert.equal(index.options.device, "cuda");
  assert.equal(index.options.embeddingConcurrency, 4);

  const queryRuntime = parseArgs([
    "query",
    "--api-key",
    "secret",
    "--model-cache",
    "/tmp/models",
    "--device",
    "CPU",
    "query text",
  ]);
  assert.equal(queryRuntime.options.apiKey, "secret");
  assert.equal(queryRuntime.options.modelCacheDir, "/tmp/models");
  assert.equal(queryRuntime.options.device, "cpu");

  const rg = parseArgs([
    "query",
    "--rg",
    "-F",
    "-i",
    "-C",
    "2",
    "-g",
    "*.ts",
    "needle",
    "src",
  ]);
  assert.equal(rg.options.rg, true);
  assert.deepEqual(rg.positionals, ["needle", "src"]);
  assert.deepEqual(rg.options.rgOptions?.extraArgs, ["-F", "-i"]);
  assert.equal(rg.options.rgOptions?.beforeContext, 2);
  assert.equal(rg.options.rgOptions?.afterContext, 2);
  assert.deepEqual(rg.options.globs, ["*.ts"]);
});

test("CLI parsers reject invalid values and normalize supported values", () => {
  assert.throws(
    () =>
      parseArgs([
        "query",
        "--endpoint",
        "https://example.test/embeddings",
        "query text",
      ]),
    /--endpoint is not supported with zg query/,
  );
  assert.equal(parseDevice("cpu"), "cpu");
  assert.equal(parseDevice("CUDA"), "cuda");
  assert.deepEqual(splitPathFilters("src/**, test/**, docs/**"), [
    "src/**",
    "test/**",
    "docs/**",
  ]);
  assert.deepEqual(
    splitPathFilters(String.raw`src/\,name.ts, test/[a,b].ts, *.{py,{cc,h}}`),
    [String.raw`src/\,name.ts`, "test/[a,b].ts", "*.{py,{cc,h}}"],
  );
  assert.equal(
    parseModifiedTime("1700000000000", "--modified-after"),
    1700000000000,
  );
  assert.throws(() => parseDevice("magic"), /Unsupported device/);
  assert.throws(
    () => parseArgs(["query", "--limit", "0", "query"]),
    /positive integer/,
  );
  assert.throws(
    () => parseArgs(["query", "--limit", "2x", "query"]),
    /positive integer/,
  );
  assert.throws(
    () => parseArgs(["query", "--preview", "huge", "query"]),
    /Unsupported preview mode/,
  );
  assert.throws(() => parseArgs(["query", "--json", "query"]), /removed/);
  assert.throws(
    () =>
      parseArgs(["query", "--embedding", "local/embeddinggemma-300m", "query"]),
    /--embedding is not supported with zg query/,
  );
  assert.throws(() => parseArgs(["--unknown"]), /Unknown command/);
});

test("MCP rg command normalization preserves quoted patterns, paths, and globs", () => {
  const braceGlob = "*.{py,cc,cpp,h,hpp}";
  const normalized = contextOptionsFromRgInput({
    root: "/repo",
    command: `rg -niF 'valid value' -g '${braceGlob}' -g '!test/**' src test`,
  });
  assert.deepEqual(normalized.queries, ["valid value"]);
  assert.deepEqual(normalized.rgPaths, ["src", "test"]);
  assert.deepEqual(normalized.globs, [braceGlob, "!test/**"]);
  assert.deepEqual(normalized.rgOptions.extraArgs, ["-i", "-F"]);
  assert.equal(normalized.limit, undefined);

  const regex = contextOptionsFromRgInput({
    root: "/repo",
    command: String.raw`rg "\\d+" src`,
  });
  assert.deepEqual(regex.queries, [String.raw`\d+`]);

  const bounded = contextOptionsFromRgInput({
    root: "/repo",
    command: `rg -ln "needle" src 2>/dev/null | head -30`,
  });
  assert.deepEqual(bounded.queries, ["needle"]);
  assert.deepEqual(bounded.rgPaths, ["src"]);
  assert.deepEqual(bounded.rgOptions.extraArgs, ["--max-count", "1"]);
  assert.equal(bounded.limit, 30);

  const defaultHead = contextOptionsFromRgInput({
    root: "/repo",
    command: `rg "needle" src | head`,
  });
  assert.equal(defaultHead.limit, 10);

  const largeBound = contextOptionsFromRgInput({
    root: "/repo",
    command: `rg "needle" src | head -1000`,
  });
  assert.equal(largeBound.limit, 1000);

  const hyphenPattern = contextOptionsFromRgInput({
    root: "/repo",
    command: `rg -n '----.*1' src`,
  });
  assert.deepEqual(hyphenPattern.queries, ["----.*1"]);
});

test(
  "MCP rg command normalization preserves unquoted Windows path separators",
  { skip: process.platform !== "win32" },
  () => {
    const normalized = contextOptionsFromRgInput({
      root: process.cwd(),
      command: String.raw`rg needle src\cli`,
    });

    assert.deepEqual(normalized.rgPaths, [String.raw`src\cli`]);
  },
);

test(
  "MCP rg command normalization rejects paths that escape through symlinks",
  { skip: process.platform === "win32" },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-managed-rg-"),
    );
    const root = join(temporaryDirectory, "repo");
    const outside = join(temporaryDirectory, "outside");
    await Promise.all([mkdir(root), mkdir(outside)]);
    await writeFile(join(outside, "patterns.txt"), "needle\n");
    await symlink(join(outside, "patterns.txt"), join(root, "patterns.txt"));
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    assert.throws(
      () =>
        contextOptionsFromRgInput({
          root,
          command: "rg -f patterns.txt .",
        }),
      /pattern file resolves outside root/,
    );
  },
);

test("MCP rg command parsing rejects shell syntax, non-rg flags, and root escapes", () => {
  for (const command of [
    "grep valid src",
    "rg valid src | tail -10",
    "rg valid src > output.txt",
    "rg $(whoami)",
    "rg 'unclosed",
    "rg valid --limit 2",
    "rg valid ../outside",
    "rg --follow valid src",
  ]) {
    assert.throws(
      () => contextOptionsFromRgInput({ root: "/repo", command }),
      /rg command|within root|unclosed/i,
      command,
    );
  }
});

test("CLI parser covers utility commands, provider controls, routes, and equals syntax", () => {
  const install = parseArgs([
    "install",
    "--target=codex, claude",
    "--target",
    "cursor",
    "--mcp-tool-timeout=30",
    "--mcp-transport=stdio",
    "--mcp-toolset=full",
    "--yes",
  ]);
  assert.equal(install.command, "install");
  assert.deepEqual(install.options.installTargets, [
    "codex",
    "claude",
    "cursor",
  ]);
  assert.equal(install.options.installMcpToolTimeoutSeconds, 30);
  assert.equal(install.options.installMcpTransport, "stdio");
  assert.equal(install.options.mcpToolset, "full");
  assert.throws(
    () => parseArgs(["install", "--mcp-token-env", "TOKEN"]),
    /requires --mcp-transport http/,
  );

  assert.throws(() => parseArgs(["serve", "--mcp"]), /removed/i);
  assert.equal(parseArgs(["-h"]).command, "help");
  assert.equal(parseArgs(["help", "query"]).helpTopic, "query");
  assert.equal(parseArgs(["help", "environment"]).helpTopic, "environment");
  assert.equal(parseArgs(["help", "env"]).helpTopic, "env");
  assert.equal(parseArgs(["-v"]).command, "version");
  assert.equal(parseArgs(["version", "-v"]).command, "version");
  assert.equal(parseArgs(["version", "--version"]).command, "version");
  assert.deepEqual(
    parseArgs(["query", "--rg", "-v", "needle"]).options.rgOptions?.extraArgs,
    ["-v"],
  );
  assert.equal(parseArgs(["index", "--drop", "--yes"]).options.drop, true);
  assert.equal(parseArgs(["status"]).command, "status");
  assert.equal(parseArgs(["status", "--check-ready"]).options.checkReady, true);
  assert.equal(
    parseArgs(["server", "status", "--check-ready"]).options.checkReady,
    true,
  );
  assert.throws(
    () => parseArgs(["query", "--check-ready", "query"]),
    /zg status or zg server status/,
  );
  assert.throws(() => parseArgs(["collections"]), /Unknown command/);
  assert.deepEqual(
    parseArgs([
      "auth",
      "grant",
      "/tmp/workspace",
      "--capability",
      "embedding",
      "--scope=workspace",
    ]),
    {
      command: "auth",
      options: {
        authAction: "grant",
        authorizationCapability: "embedding",
        authorizationScope: "workspace",
      },
      positionals: ["/tmp/workspace"],
    },
  );
  assert.equal(
    parseArgs(["query", "--allow-remote", "query"]).options.allowRemote,
    true,
  );
  assert.throws(
    () => parseArgs(["query", "--allow-remote=once", "query"]),
    /does not accept a value/,
  );
  assert.throws(
    () => parseArgs(["query", "--allow-remote", "workspace", "query"]),
    /does not accept a value/,
  );

  const query = parseArgs([
    "query",
    "--debug",
    "--human",
    "--preview=full",
    "--refresh=off",
    "--prefer-symbol",
    "--home",
    "/tmp/home",
    "--hybrid",
    "zero",
    "--fts",
    "one",
    "--fts",
    "two",
    "--vector",
    "three",
    "--color",
    "auto",
    "--glob=src/**",
    "--glob",
    "docs/**",
    "--glob",
    "!dist/**",
    "--type",
    "ts",
    "--modified-before",
    "2026-01-01T00:00:00Z",
    "--symbol-type",
    "class",
    "--",
    "-literal-query",
  ]);
  assert.equal(query.options.debug, true);
  assert.deepEqual(query.options.hybridQueries, ["zero"]);
  assert.deepEqual(query.options.routes, [
    { mode: "fts", query: "one" },
    { mode: "fts", query: "two" },
    { mode: "vector", query: "three" },
  ]);
  assert.deepEqual(query.options.symbolTypes, ["class"]);
  assert.deepEqual(query.options.globs, ["src/**", "docs/**", "!dist/**"]);
  assert.deepEqual(query.options.fileTypes, ["ts"]);
  assert.deepEqual(query.positionals, ["-literal-query"]);
  assert.equal(parseArgs(["index", "--debug"]).options.debug, true);
});

test("CLI parser covers managed rg long and short compatibility options", () => {
  const long = parseArgs([
    "query",
    "--rg",
    "--ignore-case",
    "--word-regexp",
    "--fixed-strings",
    "--hidden",
    "--encoding=utf8",
    "--engine",
    "auto",
    "--no-ignore",
    "--recursive",
    "--line-number",
    "--with-filename",
    "--regexp=first",
    "--regexp",
    "second",
    "--context=2",
    "--before-context",
    "3",
    "--after-context=4",
    "--glob=*.ts",
    "--glob",
    "!*.test.ts",
    "--iglob=*.MD",
    "--type=ts",
    "--type-not",
    "json",
    "--ignore-file=.rgignore",
    "--max-depth=4",
    "--max-filesize",
    "2M",
    "--follow",
    "--file=patterns.txt",
    "--line-regexp",
    "--invert-match",
    "--max-count=3",
    "--modified-after",
    "2026-01-01",
    "needle",
  ]);
  assert.deepEqual(long.options.rgOptions?.patterns, ["first", "second"]);
  assert.equal(long.options.rgOptions?.beforeContext, 3);
  assert.equal(long.options.rgOptions?.afterContext, 4);
  assert.deepEqual(long.options.globs, ["*.ts", "!*.test.ts"]);
  assert.deepEqual(long.options.insensitiveGlobs, ["*.MD"]);
  assert.deepEqual(long.options.fileTypes, ["ts"]);
  assert.deepEqual(long.options.excludedFileTypes, ["json"]);
  assert.equal(long.options.hidden, true);
  assert.equal(long.options.noIgnore, true);
  assert.deepEqual(long.options.ignoreFiles, [".rgignore"]);
  assert.equal(long.options.maxDepth, 4);
  assert.equal(long.options.maxFileSizeBytes, 2 * 1024 * 1024);
  assert.equal(long.options.follow, true);
  assert.equal(long.options.modifiedAfter, new Date(2026, 0, 1).getTime());
  assert.deepEqual(long.options.rgOptions?.patternFiles, ["patterns.txt"]);
  assert.ok(long.options.rgOptions?.extraArgs?.includes("--encoding"));
  assert.ok(long.options.rgOptions?.extraArgs?.includes("--fixed-strings"));
  assert.ok(long.options.rgOptions?.extraArgs?.includes("--ignore-case"));
  assert.ok(long.options.rgOptions?.extraArgs?.includes("--word-regexp"));

  const short = parseArgs([
    "query",
    "--rg",
    "-nHFiwPSsuvxUzL",
    "-einline",
    "-g*.js",
    "-tts",
    "-T",
    "json",
    "-Eutf8",
    "-m2",
    "-j1",
    "-A2",
    "-B",
    "3",
    "-C4",
    "needle",
  ]);
  assert.ok(short.options.rgOptions?.extraArgs?.includes("-F"));
  assert.ok(short.options.rgOptions?.extraArgs?.includes("-i"));
  assert.ok(short.options.rgOptions?.extraArgs?.includes("-w"));
  assert.deepEqual(short.options.rgOptions?.patterns, ["inline"]);
  assert.equal(short.options.rgOptions?.beforeContext, 4);
  assert.equal(short.options.rgOptions?.afterContext, 4);
  assert.deepEqual(short.options.globs, ["*.js"]);
  assert.deepEqual(short.options.fileTypes, ["ts"]);
  assert.deepEqual(short.options.excludedFileTypes, ["json"]);
  assert.equal(short.options.follow, true);
});

test("CLI shape validation rejects every incompatible command family", () => {
  const invalid = [
    [["serve"], /removed/i],
    [["serve", "--mcp", "--fts", "query"], /removed/i],
    [["query", "--mcp", "query"], /Unknown option/],
    [["query", "--collection", "docs", "query"], /Unknown option/],
    [["index", "--fts", "query"], /only be used with zg query/],
    [["status", "--rg", "query"], /only be used with zg query/],
    [["index", "--refresh", "off"], /only be used with zg query/],
    [["query", "--rg", "query", "--fts", "query"], /cannot be combined/],
    [["query", "--rg", "query", "--hybrid", "query"], /cannot be combined/],
    [["query", "--rg", "query", "--fuse"], /cannot be combined/],
    [
      ["query", "--rg", "query", "--preview", "short"],
      /not supported with --rg/,
    ],
    [["query", "--rg", "query", "--trace"], /cannot be combined/],
    [["query", "--rg", "query", "--prefer-symbol"], /indexed symbol options/],
    [["query", "--rg", "query", "--refresh", "off"], /indexed refresh options/],
    [
      ["query", "--rg", "query", "--embedding-concurrency", "2"],
      /indexed refresh options/,
    ],
    [["query", "--reset-paths", "query"], /only be used with zg index/],
    [["query", "--ignore-case", "query"], /only be used with --rg/],
    [["query", "--hidden", "query"], /index commands or zg query --rg/],
    [["install", "-g", "src/**"], /query or index commands/],
    [["query", "--drop", "query"], /only be used with zg index/],
    [["index", "--drop", "--rebuild"], /cannot be combined/],
    [["index", "--drop", "-g", "src/**"], /cannot be combined/],
    [["uninstall", "--force"], /only be used with zg install/],
    [["status", "--target", "codex"], /only be used with zg install/],
    [["auth", "grant", "--scope", "session"], /only workspace scope/],
    [["status", "--allow-remote"], /query or index commands/],
  ];
  for (const [args, message] of invalid) {
    assert.throws(() => parseArgs(args), message, args.join(" "));
  }

  for (const args of [
    ["query", "--color", "sometimes", "query"],
    ["query", "--symbol-type", "method", "query"],
    ["query", "--modified-after", "not-a-date", "query"],
    ["query", "--modified-after", "999999999999999999999", "query"],
    ["query", "--modified-after", "2026-13-40", "query"],
    ["query", "--rg", "--context", "-1", "query"],
    ["query", "--count", "query"],
    ["query", "--rg", "-l", "query"],
    ["query", "--rg", "-q", "query"],
    ["query", "--rg", "--stats", "query"],
    ["query", "--include", "src/**", "query"],
    ["query", "--fts"],
    ["install", "--target"],
  ]) {
    assert.throws(() => parseArgs(args), undefined, args.join(" "));
  }
});

test("color, range, highlighting, and error helpers are deterministic", () => {
  assert.equal(colorModeFromArgs(["--color", "always"]), "always");
  assert.equal(colorModeFromArgs(["--no-color"]), "never");
  assert.equal(shouldUseColor({ color: "always" }), true);
  assert.equal(shouldUseColor({ color: "never" }), false);
  assert.equal(createHighlighter("symbol", false)("symbol"), "symbol");
  assert.match(
    createHighlighter("find ImportantSymbol", true)("ImportantSymbol here"),
    /\x1b\[1;33m/,
  );
  assert.equal(
    rangeLabel({
      kind: "text",
      startLine: 2,
      endLine: 4,
      startOffset: 0,
      endOffset: 10,
    }),
    "2-4",
  );
  assert.equal(
    rangeLabel({ kind: "byte", startOffset: 2, endOffset: 9 }),
    "bytes:2-9",
  );
  assert.equal(rangeLabel({ kind: "page", page: 3 }), "page:3");
  assert.equal(rangeLabel({ kind: "file" }), "file");
  assert.equal(rangeStartLine({ kind: "file" }), 0);
  assert.equal(
    errorDetails([detail("operation", "index"), detail("empty", undefined)]),
    "operation=index",
  );
  const error = new EngineError("failed", {
    code: "ZVEC_GREP.ENGINE.TEST.FAILURE",
  });
  assert.equal(isEngineError(error), true);
  assert.equal(isEngineError(new Error("failed")), false);
});

test("file, model, content, and entity helpers classify inputs", () => {
  assert.deepEqual(detectFileType("Dockerfile"), {
    kind: "code",
    format: "dockerfile",
  });
  assert.deepEqual(detectFileType("source.TS"), {
    kind: "code",
    format: "typescript",
  });
  assert.deepEqual(detectFileType("README.md"), {
    kind: "text",
    format: "markdown",
  });
  assert.deepEqual(detectFileType("image.png"), {
    kind: "image",
    format: "png",
  });
  assert.equal(detectFileType("archive.zip"), null);
  assert.deepEqual(detectFileType("NOTICE"), { kind: "text", format: "text" });
  assert.equal(listEmbeddingModels().length, 14);
  assert.equal(
    getEmbeddingModelCatalogEntry("local/embeddinggemma-300m")?.dimension,
    768,
  );
  assert.equal(getEmbeddingModelCatalogEntry("missing"), undefined);
  assert.equal(
    getEmbeddingModelCatalogEntry("qwen/text-embedding-v4")?.dimension,
    1024,
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/qwen3-embedding-0.6b")?.reference,
    "local/qwen3-embedding-0.6b",
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/bge-small-en-v1.5")?.backend,
    "transformers-js",
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/bge-small-en-v1.5")?.maxBatchSize,
    4,
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/all-minilm-l6-v2")?.maxInputTokens,
    256,
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/all-minilm-l6-v2")?.maxBatchSize,
    4,
  );
  assert.deepEqual(
    {
      backend: getEmbeddingModelCatalogEntry("local/potion-retrieval-32m")
        ?.backend,
      modelFile: getEmbeddingModelCatalogEntry("local/potion-retrieval-32m")
        ?.modelFile,
      dimension: getEmbeddingModelCatalogEntry("local/potion-retrieval-32m")
        ?.dimension,
      maxInputTokens: getEmbeddingModelCatalogEntry(
        "local/potion-retrieval-32m",
      )?.maxInputTokens,
      defaultConcurrency: getEmbeddingModelCatalogEntry(
        "local/potion-retrieval-32m",
      )?.defaultConcurrency,
    },
    {
      backend: "model2vec",
      modelFile: "model.safetensors",
      dimension: 512,
      maxInputTokens: 1024,
      defaultConcurrency: 2,
    },
  );
  assert.deepEqual(
    {
      modelFile: getEmbeddingModelCatalogEntry("local/potion-multilingual-128m")
        ?.modelFile,
      dimension: getEmbeddingModelCatalogEntry("local/potion-multilingual-128m")
        ?.dimension,
      maxInputTokens: getEmbeddingModelCatalogEntry(
        "local/potion-multilingual-128m",
      )?.maxInputTokens,
      defaultConcurrency: getEmbeddingModelCatalogEntry(
        "local/potion-multilingual-128m",
      )?.defaultConcurrency,
    },
    {
      modelFile: "model.safetensors",
      dimension: 256,
      maxInputTokens: 1024,
      defaultConcurrency: 2,
    },
  );
  assert.deepEqual(
    {
      modelFile: getEmbeddingModelCatalogEntry("local/potion-code-16m-v2")
        ?.modelFile,
      dimension: getEmbeddingModelCatalogEntry("local/potion-code-16m-v2")
        ?.dimension,
      maxInputTokens: getEmbeddingModelCatalogEntry("local/potion-code-16m-v2")
        ?.maxInputTokens,
      defaultConcurrency: getEmbeddingModelCatalogEntry(
        "local/potion-code-16m-v2",
      )?.defaultConcurrency,
    },
    {
      modelFile: "model.safetensors",
      dimension: 256,
      maxInputTokens: 1024,
      defaultConcurrency: 2,
    },
  );
  assert.deepEqual(
    {
      prefix: getEmbeddingModelCatalogEntry("local/multilingual-e5-small")
        ?.queryPrefix,
      dimension: getEmbeddingModelCatalogEntry("local/multilingual-e5-small")
        ?.dimension,
    },
    { prefix: "query: ", dimension: 384 },
  );
  assert.deepEqual(
    {
      pooling: getEmbeddingModelCatalogEntry(
        "local/jina-embeddings-v2-base-code",
      )?.pooling,
      maxInputTokens: getEmbeddingModelCatalogEntry(
        "local/jina-embeddings-v2-base-code",
      )?.maxInputTokens,
    },
    { pooling: "mean", maxInputTokens: 8192 },
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/gte-modernbert-base")?.pooling,
    "cls",
  );
  assert.equal(
    getEmbeddingModelCatalogEntry("local/nomic-embed-text-v1.5")
      ?.documentPrefix,
    "search_document: ",
  );
  assert.equal(isTextContent({ kind: "text", text: "hello" }), true);
  assert.equal(
    isImageContent({ kind: "image", data: new Uint8Array([1]), format: "png" }),
    true,
  );
  assert.equal(makeEntityId("file", 3), makeEntityId("file", 3));
  assert.notEqual(makeEntityId("file", 3), makeEntityId("file", 4));
});

test("glob and path helpers cover literal, wildcard, and descendant matching", () => {
  assert.equal(normalizePathPattern("./src\\**\\*.ts"), "src/**/*.ts");
  assert.equal(normalizePathForMatch("src\\file.ts"), "src/file.ts");
  assert.equal(isAbsolutePathPattern("/tmp/file"), true);
  assert.equal(isAbsolutePathPattern("C:/repo/file"), true);
  assert.equal(hasPathGlob("src/**"), true);
  assert.equal(pathPatternMatches("src/**", "src/nested/file.ts"), true);
  assert.equal(pathPatternMatches("*.ts", "src/file.ts"), true);
  assert.equal(pathPatternMatches("*.[chH]", "src/file.H"), true);
  assert.equal(
    pathPatternMatchesCaseInsensitive("*.md", "docs/README.MD"),
    true,
  );
  assert.equal(pathPatternMatches("src", "src/nested/file.ts"), true);
  assert.equal(pathPatternMatches("", "src/file.ts"), false);
  assert.equal(ripgrepGlobMatches("README.md", "docs/README.md"), true);
  assert.equal(ripgrepGlobMatches("*.{ts,md}", "docs/README.md"), true);
  assert.equal(ripgrepGlobMatches("*.{ts,md}", "src/main.ts"), true);
  assert.equal(ripgrepGlobMatches("*.{ts,md}", "src/main.js"), false);
  assert.equal(
    ripgrepGlobMatches("src/{generated,{api,core}}/**", "src/core/main.ts"),
    true,
  );
  assert.equal(ripgrepGlobMatches("src", "src/nested/file.ts"), false);
  assert.equal(
    ripgrepGlobMatchesCaseInsensitive("README.MD", "docs/readme.md"),
    true,
  );
  assert.equal(
    matchesFileSelection(
      "src/main.ts",
      { globs: ["!*.ts", "main.ts"] },
      { include: [], exclude: [] },
    ),
    true,
  );
  assert.equal(
    pathPatternMightMatchDescendant("src/generated/**", "src"),
    true,
  );
  const parent = normalizePath("fixtures");
  assert.equal(isPathInside(parent, resolve(parent, "child")), true);
  assert.equal(isPathInside(parent, resolve(parent, "..", "other")), false);
  assert.equal(toDisplayPath(parent).includes("\\"), false);
});

test("file type filters accept extension aliases for ripgrep types", async () => {
  const types = await resolveFileTypePatterns([".h", "cc"], undefined);
  assert.equal(matchesFileSelection("include/api.h", {}, types), true);
  assert.equal(matchesFileSelection("src/main.cc", {}, types), true);
  assert.equal(matchesFileSelection("src/main.ts", {}, types), false);
});

test("timing helpers aggregate entries and concurrent work", async () => {
  const timings = new TimingCollector();
  timings.add("scan", 4);
  timings.add("scan", 6, 2);
  timings.add("", 10);
  timings.add("bad", Number.NaN);
  assert.equal(
    timings.timeSync("sync", () => 42),
    42,
  );
  await timings.time("async", async () => "done");
  const concurrent = new ConcurrentTiming(timings, "parallel");
  await Promise.all([
    concurrent.time(async () => 1),
    concurrent.time(async () => 2),
  ]);
  const entries = timings.entries();
  assert.deepEqual(
    entries.find((entry) => entry.name === "scan"),
    { name: "scan", durationMs: 10, count: 3 },
  );
  assert.ok(entries.some((entry) => entry.name === "parallel"));
});
