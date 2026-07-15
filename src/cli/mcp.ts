import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import {
  createZvecGrep,
  type CodeSymbolType,
  type CreateZvecGrepOptions,
  type ZvecGrep,
  type ZvecGrepContextOptions,
  type ZvecGrepContextResult,
  type ZvecGrepSearchOptions,
} from "../index.js";
import { parseModifiedTime, splitPathFilters } from "./args.js";
import { readPackageVersion } from "./version.js";

const stringListInputSchema = z
  .union([z.string(), z.array(z.string())])
  .optional();
const timeInputSchema = z
  .union([z.number().int().nonnegative(), z.string()])
  .optional();
const codeSymbolTypeSchema = z.enum([
  "module",
  "class",
  "interface",
  "function",
  "value",
  "alias",
]);

type StringListInput = z.infer<typeof stringListInputSchema>;
type TimeInput = z.infer<typeof timeInputSchema>;

export async function runMcpServer(
  options: CreateZvecGrepOptions,
): Promise<void> {
  const zvecGrep = await createZvecGrep(options);
  const server = createMcpServer(zvecGrep);
  const transport = new StdioServerTransport();

  const close = async () => {
    await zvecGrep.close();
    await server.close();
  };
  process.once("SIGINT", () => {
    void close().finally(() => process.exit(130));
  });
  process.once("SIGTERM", () => {
    void close().finally(() => process.exit(143));
  });

  await server.connect(transport);
}

function createMcpServer(zvecGrep: ZvecGrep): McpServer {
  const server = new McpServer(
    {
      name: "zvec-grep",
      version: readPackageVersion(),
    },
    {
      instructions: [
        "Use zvec-grep for repository code search before grep, rg, or broad file reads.",
        "Use zvec_grep_search for indexed semantic and lexical retrieval, and zvec_grep_rg for explicit no-index lexical search.",
        "Index management and status inspection are CLI-only operations.",
      ].join(" "),
    },
  );

  server.registerTool(
    "zvec_grep_search",
    {
      title: "zvec-grep indexed search",
      description:
        "Search an indexed repository with zvec-grep hybrid semantic and lexical retrieval. Like the CLI, this may refresh a stale anonymous index unless autoUpdate is false.",
      inputSchema: {
        query: z
          .string()
          .optional()
          .describe("Natural-language or exact search query."),
        queries: stringListInputSchema.describe(
          "Multiple query groups. Use this for related concepts.",
        ),
        fts: stringListInputSchema.describe(
          "Exact lexical anchors, such as symbols, flags, or error messages.",
        ),
        vector: stringListInputSchema.describe(
          "Explicit semantic/vector-only queries.",
        ),
        root: z
          .string()
          .optional()
          .describe(
            "Repository root. Defaults to the MCP server working directory.",
          ),
        collection: z
          .string()
          .optional()
          .describe(
            "Named collection to query instead of the anonymous repository index.",
          ),
        limit: z
          .number()
          .int()
          .positive()
          .max(50)
          .optional()
          .describe("Maximum returned items per query/group."),
        include: stringListInputSchema.describe(
          "Glob filters for paths to include. Accepts an array or CLI-style comma-separated string.",
        ),
        exclude: stringListInputSchema.describe(
          "Glob filters for paths to exclude. Accepts an array or CLI-style comma-separated string.",
        ),
        preferSymbol: z
          .boolean()
          .optional()
          .describe("Prefer exact indexed symbols when query names a symbol."),
        symbolTypes: z
          .array(codeSymbolTypeSchema)
          .default([])
          .describe("Restrict indexed results to symbol types."),
        modifiedAfter: timeInputSchema.describe(
          "Only query files modified after this time. Accepts epoch milliseconds or a parseable date string.",
        ),
        modifiedBefore: timeInputSchema.describe(
          "Only query files modified before this time. Accepts epoch milliseconds or a parseable date string.",
        ),
        autoUpdate: z
          .boolean()
          .optional()
          .describe(
            "Refresh an existing stale anonymous index before query. Defaults to CLI behavior: true.",
          ),
        embeddingConcurrency: z
          .number()
          .int()
          .positive()
          .max(64)
          .optional()
          .describe("Embedding task concurrency for automatic index refresh."),
        trace: z
          .boolean()
          .optional()
          .describe("Include per-hit search trace in structured output."),
        maxContentChars: z
          .number()
          .int()
          .positive()
          .max(6000)
          .default(1200)
          .describe("Maximum source characters to include per hit."),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    async (input) => {
      const result = await zvecGrep.context(
        contextOptionsFromSearchInput(input),
      );
      return toolResult(contextText(result, input.maxContentChars), {
        result: simplifyContextResult(result, input.maxContentChars),
      });
    },
  );

  server.registerTool(
    "zvec_grep_rg",
    {
      title: "zvec-grep no-index lexical search",
      description:
        "Run explicit no-index lexical search through zvec-grep managed ripgrep output.",
      inputSchema: {
        pattern: z
          .string()
          .optional()
          .describe("Regex or literal pattern to search for."),
        patterns: stringListInputSchema.describe(
          "Multiple regex or literal patterns to search for.",
        ),
        root: z
          .string()
          .optional()
          .describe(
            "Repository root. Defaults to the MCP server working directory.",
          ),
        paths: stringListInputSchema.describe(
          "Optional paths to search within the root.",
        ),
        fixedStrings: z
          .boolean()
          .optional()
          .describe("Treat pattern as a literal string."),
        ignoreCase: z
          .boolean()
          .optional()
          .describe("Search case-insensitively."),
        wordRegexp: z.boolean().optional().describe("Only match whole words."),
        context: z
          .number()
          .int()
          .nonnegative()
          .max(20)
          .optional()
          .describe("Context lines before and after each match."),
        beforeContext: z
          .number()
          .int()
          .nonnegative()
          .max(20)
          .optional()
          .describe("Context lines before each match."),
        afterContext: z
          .number()
          .int()
          .nonnegative()
          .max(20)
          .optional()
          .describe("Context lines after each match."),
        hidden: z
          .boolean()
          .optional()
          .describe("Search hidden files and directories."),
        glob: stringListInputSchema.describe(
          "ripgrep glob filters, for example '*.ts' or '!dist/**'. Accepts an array or CLI-style comma-separated string.",
        ),
        limit: z
          .number()
          .int()
          .positive()
          .max(200)
          .optional()
          .describe("Maximum returned matches."),
        maxContentChars: z
          .number()
          .int()
          .positive()
          .max(6000)
          .default(1200)
          .describe("Maximum source characters to include per hit."),
      },
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: false,
      },
    },
    async (input) => {
      const result = await zvecGrep.context(contextOptionsFromRgInput(input));
      return toolResult(contextText(result, input.maxContentChars), {
        result: simplifyContextResult(result, input.maxContentChars),
      });
    },
  );

  return server;
}

function contextOptionsFromSearchInput(input: {
  query?: string;
  queries?: StringListInput;
  fts?: StringListInput;
  vector?: StringListInput;
  root?: string;
  collection?: string;
  limit?: number;
  include?: StringListInput;
  exclude?: StringListInput;
  preferSymbol?: boolean;
  symbolTypes: CodeSymbolType[];
  modifiedAfter?: TimeInput;
  modifiedBefore?: TimeInput;
  autoUpdate?: boolean;
  embeddingConcurrency?: number;
  trace?: boolean;
}): ZvecGrepContextOptions {
  const queries = [
    ...normalizeQueryList(input.query),
    ...normalizeQueryList(input.queries),
  ];
  const fts = normalizeQueryList(input.fts);
  const vector = normalizeQueryList(input.vector);
  if (queries.length === 0 && fts.length === 0 && vector.length === 0) {
    throw new Error(
      "zvec_grep_search requires query, queries, fts, or vector.",
    );
  }

  const includePaths = normalizePathFilters(input.include);
  const excludePaths = normalizePathFilters(input.exclude);
  return {
    queries: queries.length > 0 ? queries : undefined,
    routes: [
      ...fts.map((query) => ({ mode: "fts" as const, query })),
      ...vector.map((query) => ({ mode: "vector" as const, query })),
    ],
    root: normalizeOptionalString(input.root),
    collection: normalizeOptionalString(input.collection),
    limit: input.limit,
    fallback: "disabled",
    autoUpdate: input.autoUpdate ?? true,
    trace: input.trace,
    preferSymbol: input.preferSymbol,
    symbolTypes: input.symbolTypes.length > 0 ? input.symbolTypes : undefined,
    includePaths: includePaths.length > 0 ? includePaths : undefined,
    excludePaths: excludePaths.length > 0 ? excludePaths : undefined,
    modifiedAfter: normalizeModifiedTime(input.modifiedAfter, "modifiedAfter"),
    modifiedBefore: normalizeModifiedTime(
      input.modifiedBefore,
      "modifiedBefore",
    ),
    embeddingConcurrency: input.embeddingConcurrency,
  };
}

function contextOptionsFromRgInput(input: {
  pattern?: string;
  patterns?: StringListInput;
  root?: string;
  paths?: StringListInput;
  fixedStrings?: boolean;
  ignoreCase?: boolean;
  wordRegexp?: boolean;
  context?: number;
  beforeContext?: number;
  afterContext?: number;
  hidden?: boolean;
  glob?: StringListInput;
  limit?: number;
}): ZvecGrepContextOptions {
  const queries = [
    ...normalizeQueryList(input.pattern),
    ...normalizeQueryList(input.patterns),
  ];
  if (queries.length === 0) {
    throw new Error("zvec_grep_rg requires pattern or patterns.");
  }

  const { includePaths, excludePaths } = pathFiltersFromRgGlobs(input.glob);
  const rgOptions: ZvecGrepSearchOptions = {
    fixedStrings: input.fixedStrings,
    ignoreCase: input.ignoreCase,
    wordRegexp: input.wordRegexp,
    beforeContext: input.beforeContext ?? input.context,
    afterContext: input.afterContext ?? input.context,
    hidden: input.hidden,
  };
  return {
    queries,
    rg: true,
    rgOptions,
    rgPaths: normalizePlainStringList(input.paths),
    root: normalizeOptionalString(input.root),
    limit: input.limit,
    includePaths,
    excludePaths,
  };
}

function pathFiltersFromRgGlobs(value: StringListInput): {
  includePaths?: string[];
  excludePaths?: string[];
} {
  const includePaths: string[] = [];
  const excludePaths: string[] = [];
  for (const glob of normalizePathFilters(value)) {
    if (glob.startsWith("!")) {
      const exclude = glob.slice(1).trim();
      if (exclude.length > 0) {
        excludePaths.push(exclude);
      }
      continue;
    }
    includePaths.push(glob);
  }

  return {
    includePaths: includePaths.length > 0 ? includePaths : undefined,
    excludePaths: excludePaths.length > 0 ? excludePaths : undefined,
  };
}

function normalizeQueryList(value: StringListInput): string[] {
  return normalizePlainStringList(value) ?? [];
}

function normalizePlainStringList(
  value: StringListInput,
): string[] | undefined {
  const items =
    value === undefined ? [] : Array.isArray(value) ? value : [value];
  const normalized = items
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
  return normalized.length > 0 ? normalized : undefined;
}

function normalizePathFilters(value: StringListInput): string[] {
  const items =
    value === undefined ? [] : Array.isArray(value) ? value : [value];
  return items.flatMap(splitPathFilters);
}

function normalizeModifiedTime(
  value: TimeInput,
  option: string,
): number | undefined {
  if (value === undefined) {
    return undefined;
  }
  return typeof value === "number" ? value : parseModifiedTime(value, option);
}

function normalizeOptionalString(
  value: string | undefined,
): string | undefined {
  const normalized = value?.trim() ?? "";
  return normalized.length > 0 ? normalized : undefined;
}

function toolResult(text: string, structuredContent: Record<string, unknown>) {
  return {
    content: [{ type: "text" as const, text }],
    structuredContent,
  };
}

function contextText(
  result: ZvecGrepContextResult,
  maxContentChars: number,
): string {
  const lines = [
    `query: ${result.query}`,
    `root: ${result.root}`,
    `source: ${result.source}`,
    `coverage: ${result.coverage}`,
    `hits: ${result.items.length}`,
  ];

  for (const item of result.items) {
    lines.push("");
    lines.push(
      `${item.file.relativePath}:${rangeLabel(item.range)} rank=${item.rank} matchedBy=${item.matchedBy}`,
    );
    if (item.outline) {
      lines.push(`outline: ${truncate(item.outline, maxContentChars)}`);
    }
    lines.push("source:");
    lines.push(truncate(item.content, maxContentChars));
  }

  return lines.join("\n");
}

function simplifyContextResult(
  result: ZvecGrepContextResult,
  maxContentChars: number,
): Record<string, unknown> {
  return {
    query: result.query,
    root: result.root,
    source: result.source,
    coverage: result.coverage,
    collection: result.collection,
    diagnostics: result.diagnostics,
    items: result.items.map((item) => ({
      kind: item.kind,
      rank: item.rank,
      file: item.file,
      range: item.range,
      excerptRange: item.excerptRange,
      outline: item.outline
        ? truncate(item.outline, maxContentChars)
        : undefined,
      content: truncate(item.content, maxContentChars),
      contentRole: item.contentRole,
      status: item.status,
      score: item.score,
      matchedBy: item.matchedBy,
      metadata: item.metadata,
      entityId: item.entityId,
      container: item.container,
      trace: item.trace,
    })),
  };
}

function rangeLabel(range: {
  kind: string;
  startLine?: number;
  endLine?: number;
  startOffset?: number;
  endOffset?: number;
}): string {
  if (
    range.kind === "text" &&
    typeof range.startLine === "number" &&
    typeof range.endLine === "number"
  ) {
    return `${range.startLine}-${range.endLine}`;
  }
  if (
    typeof range.startOffset === "number" &&
    typeof range.endOffset === "number"
  ) {
    return `${range.startOffset}-${range.endOffset}`;
  }
  return range.kind;
}

function truncate(value: string, maxChars: number): string {
  if (value.length <= maxChars) {
    return value;
  }

  return `${value.slice(0, maxChars)}\n...[truncated ${value.length - maxChars} chars]`;
}
