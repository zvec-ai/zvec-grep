import assert from "node:assert/strict";
import { resolve } from "node:path";
import test from "node:test";
import {
  parseArgs,
  parseLlamaGpu,
  parseModifiedTime,
  splitPathFilters,
} from "../../dist/cli/args.js";
import { colorModeFromArgs } from "../../dist/cli/errors.js";
import {
  createHighlighter,
  shouldUseColor,
} from "../../dist/cli/format/highlight.js";
import { rangeLabel, rangeStartLine } from "../../dist/cli/format/range.js";
import { detail, errorDetails } from "../../dist/engine/errors/details.js";
import { EngineError, isEngineError } from "../../dist/engine/errors/index.js";
import { makeEntityId } from "../../dist/engine/extraction/ids.js";
import { detectFileType } from "../../dist/engine/files/file-type.js";
import {
  getEmbeddingModelCatalogEntry,
  getEmbeddingModelCatalogEntryByRef,
  listEmbeddingModels,
} from "../../dist/engine/models/catalog.js";
import { isImageContent, isTextContent } from "../../dist/engine/types.js";
import {
  hasPathGlob,
  isAbsolutePathPattern,
  normalizePathForMatch,
  normalizePathPattern,
  pathPatternMatches,
  pathPatternMightMatchDescendant,
} from "../../dist/engine/utils/glob.js";
import {
  isPathInside,
  normalizePath,
  toDisplayPath,
} from "../../dist/engine/utils/path.js";
import {
  ConcurrentTiming,
  TimingCollector,
} from "../../dist/engine/utils/timing.js";

test("CLI argument parser handles command, provider, path, and rg options", () => {
  const parsed = parseArgs([
    "--human",
    "--trace",
    "--preview",
    "short",
    "--limit",
    "7",
    "--embedding",
    "qwen/text-embedding-v4",
    "--api-key",
    "secret",
    "--endpoint",
    "https://example.test/embeddings",
    "--include",
    "src/**,test/**",
    "--exclude=dist/**",
    "--modified-after",
    "2026-01-01",
    "query text",
  ]);
  assert.deepEqual(parsed.positionals, ["query text"]);
  assert.equal(parsed.options.human, true);
  assert.equal(parsed.options.trace, true);
  assert.equal(parsed.options.preview, "short");
  assert.equal(parsed.options.limit, 7);
  assert.equal(parsed.options.embedding, "qwen/text-embedding-v4");
  assert.equal(parsed.options.endpoint, "https://example.test/embeddings");
  assert.deepEqual(parsed.options.includePaths, ["src/**", "test/**"]);
  assert.deepEqual(parsed.options.excludePaths, ["dist/**"]);
  assert.equal(typeof parsed.options.modifiedAfter, "number");

  const rg = parseArgs([
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
  assert.equal(rg.options.rgOptions?.fixedStrings, true);
  assert.equal(rg.options.rgOptions?.ignoreCase, true);
  assert.equal(rg.options.rgOptions?.beforeContext, 2);
  assert.equal(rg.options.rgOptions?.afterContext, 2);
  assert.deepEqual(rg.options.includePaths, ["*.ts"]);
});

test("CLI parsers reject invalid values and normalize supported values", () => {
  assert.equal(parseLlamaGpu("off"), false);
  assert.equal(parseLlamaGpu("CUDA"), "cuda");
  assert.deepEqual(splitPathFilters("src/**, test/**, docs/**"), [
    "src/**",
    "test/**",
    "docs/**",
  ]);
  assert.equal(
    parseModifiedTime("1700000000000", "--modified-after"),
    1700000000000,
  );
  assert.throws(() => parseLlamaGpu("magic"), /Unsupported llama GPU mode/);
  assert.throws(() => parseArgs(["--limit", "0", "query"]), /positive integer/);
  assert.throws(
    () => parseArgs(["--preview", "huge", "query"]),
    /Unsupported preview mode/,
  );
  assert.throws(() => parseArgs(["--json", "query"]), /removed/);
  assert.throws(() => parseArgs(["--unknown"]), /Unknown option/);
});

test("CLI parser covers utility commands, provider controls, routes, and equals syntax", () => {
  const install = parseArgs([
    "install",
    "--target=codex, claude",
    "--target",
    "cursor",
    "--mcp-tool-timeout=30",
    "--yes",
  ]);
  assert.equal(install.options.install, true);
  assert.deepEqual(install.options.installTargets, [
    "codex",
    "claude",
    "cursor",
  ]);
  assert.equal(install.options.installMcpToolTimeoutSeconds, 30);

  assert.equal(parseArgs(["serve", "--mcp"]).options.serve, true);
  assert.equal(parseArgs(["-h"]).options.help, true);
  assert.equal(parseArgs(["-v"]).options.version, true);
  assert.equal(parseArgs(["--disable-index"]).options.disableIndex, true);
  assert.equal(parseArgs(["--status"]).options.status, true);
  assert.equal(parseArgs(["--collections"]).options.collections, true);

  const query = parseArgs([
    "--debug",
    "--human",
    "--preview=full",
    "--rebuild",
    "--force",
    "--no-fallback",
    "--no-auto-update",
    "--prefer-symbol",
    "--collection",
    "docs",
    "--home",
    "/tmp/home",
    "--model-cache",
    "/tmp/models",
    "--gpu",
    "--no-gpu",
    "--llama-gpu",
    "vulkan",
    "--embedding-parallelism",
    "3",
    "--embedding-concurrency",
    "4",
    "--fts",
    "one",
    "two",
    "--vector",
    "three",
    "--color",
    "auto",
    "--include=src/**",
    "--include",
    "docs/**",
    "--exclude",
    "dist/**",
    "--modified-before",
    "2026-01-01T00:00:00Z",
    "--symbol-type",
    "class",
    "--",
    "-literal-query",
  ]);
  assert.equal(query.options.debug, true);
  assert.equal(query.options.llamaGpu, "vulkan");
  assert.equal(query.options.embeddingParallelism, 3);
  assert.deepEqual(query.options.routes, [
    { mode: "fts", query: "one" },
    { mode: "fts", query: "two" },
    { mode: "vector", query: "three" },
  ]);
  assert.deepEqual(query.options.symbolTypes, ["class"]);
  assert.deepEqual(query.positionals, ["-literal-query"]);
});

test("CLI parser covers managed rg long and short compatibility options", () => {
  const long = parseArgs([
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
    "needle",
  ]);
  assert.deepEqual(long.options.rgOptions?.patterns, ["first", "second"]);
  assert.equal(long.options.rgOptions?.beforeContext, 3);
  assert.equal(long.options.rgOptions?.afterContext, 4);
  assert.deepEqual(long.options.includePaths, ["*.ts"]);
  assert.deepEqual(long.options.excludePaths, ["*.test.ts"]);
  assert.ok(long.options.rgOptions?.extraArgs?.includes("--encoding"));

  const short = parseArgs([
    "--rg",
    "-nHFiwPSsuUz",
    "-einline",
    "-g*.js",
    "-tts",
    "-T",
    "json",
    "-Eutf8",
    "-A2",
    "-B",
    "3",
    "-C4",
    "needle",
  ]);
  assert.equal(short.options.rgOptions?.fixedStrings, true);
  assert.equal(short.options.rgOptions?.ignoreCase, true);
  assert.equal(short.options.rgOptions?.wordRegexp, true);
  assert.deepEqual(short.options.rgOptions?.patterns, ["inline"]);
  assert.equal(short.options.rgOptions?.beforeContext, 4);
  assert.equal(short.options.rgOptions?.afterContext, 4);
  assert.deepEqual(short.options.includePaths, ["*.js"]);
});

test("CLI shape validation rejects every incompatible command family", () => {
  const invalid = [
    [["--index", "--collections"], /cannot be used together/],
    [["install", "--index"], /cannot be combined/],
    [["serve", "--mcp", "--status"], /cannot be combined/],
    [["serve"], /requires --mcp/],
    [["--mcp"], /only be used with zg serve/],
    [["--disable-index", "--collections"], /cannot be used together/],
    [["--disable-index", "--index"], /cannot be used together/],
    [["--disable-index", "--status"], /cannot be used together/],
    [["--status", "--collections"], /cannot be used together/],
    [["--status", "--index"], /cannot be used together/],
    [["--status", "--collection", "docs"], /does not accept/],
    [["--index", "--collection", "docs"], /does not accept/],
    [["--disable-index", "--collection", "docs"], /does not accept/],
    [["--rg", "--collection", "docs"], /does not accept/],
    [["--collections", "--collection", "docs"], /cannot be used together/],
    [["install", "--collection", "docs"], /does not accept/],
    [["--mcp-tool-timeout", "10"], /only be used with zg install/],
    [["serve", "--mcp", "--collection", "docs"], /does not accept/],
    [["--index", "--fts", "query"], /only be used with query/],
    [["--status", "--rg"], /only be used with query/],
    [["--collections", "--preview", "short"], /only be used with query/],
    [["--index", "--no-auto-update"], /only be used with query/],
    [["--rg", "--fts", "query"], /cannot be combined/],
    [["--rg", "--preview", "short"], /not supported with --rg/],
    [["--rg", "--no-fallback"], /cannot be combined/],
    [["--rg", "--trace"], /cannot be combined/],
    [["--rg", "--prefer-symbol"], /indexed symbol options/],
    [["--reset-paths"], /only be used with --index/],
    [["--ignore-case"], /only be used with --rg/],
    [["serve", "--mcp", "--fts", "query"], /only be used with query/],
  ];
  for (const [args, message] of invalid) {
    assert.throws(() => parseArgs(args), message, args.join(" "));
  }

  for (const args of [
    ["--color", "sometimes"],
    ["--symbol-type", "method"],
    ["--modified-after", "not-a-date"],
    ["--modified-after", "999999999999999999999"],
    ["--modified-after", "2026-13-40"],
    ["--context", "-1"],
    ["--count"],
    ["--rg", "-l"],
    ["--rg", "-q"],
    ["--fts"],
    ["--target"],
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
  assert.equal(listEmbeddingModels().length, 2);
  assert.equal(
    getEmbeddingModelCatalogEntry("local/embeddinggemma-300m")?.dimension,
    768,
  );
  assert.equal(getEmbeddingModelCatalogEntry("missing"), undefined);
  assert.equal(
    getEmbeddingModelCatalogEntryByRef({
      provider: "local",
      model: "qwen3-embedding-0.6b",
    })?.dimension,
    1024,
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
  assert.equal(pathPatternMatches("src", "src/nested/file.ts"), true);
  assert.equal(pathPatternMatches("", "src/file.ts"), false);
  assert.equal(
    pathPatternMightMatchDescendant("src/generated/**", "src"),
    true,
  );
  const parent = normalizePath("fixtures");
  assert.equal(isPathInside(parent, resolve(parent, "child")), true);
  assert.equal(isPathInside(parent, resolve(parent, "..", "other")), false);
  assert.equal(toDisplayPath(parent).includes("\\"), false);
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
