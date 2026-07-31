import type { CodeSymbolType, ZvecGrepContextOptions } from "../index.js";
import { parseModifiedTime, splitPathFilters } from "../cli/args.js";
import { parseManagedRgCommand } from "../cli/managed-rg.js";
import type {
  StringListInput,
  TimeInput,
  ZvecGrepRgInput,
  ZvecGrepSearchInput,
} from "./schemas.js";

export type NormalizedSearchInput = {
  root: string;
  apiKey?: string;
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
  queries?: string[];
  routes: Array<{ mode: "fts" | "vector"; query: string }>;
  fuse?: boolean;
  limit?: number;
  freshness: "eventual" | "wait_for_fresh";
  autoUpdate: boolean;
  preferSymbol?: boolean;
  symbolTypes?: CodeSymbolType[];
  globs?: string[];
  insensitiveGlobs?: string[];
  fileTypes?: string[];
  excludedFileTypes?: string[];
  hidden?: boolean;
  noIgnore?: boolean;
  ignoreFiles?: string[];
  maxDepth?: number;
  maxFileSizeBytes?: number;
  follow?: boolean;
  embeddingConcurrency?: number;
  modifiedAfter?: number;
  modifiedBefore?: number;
  trace?: boolean;
};

export function normalizeSearchInput(
  input: ZvecGrepSearchInput,
): NormalizedSearchInput {
  const common = normalizeSearchFields(input);
  return {
    root: input.root,
    apiKey: input.apiKey,
    device: input.device,
    ...common,
    freshness: input.freshness,
    autoUpdate: input.autoUpdate,
  };
}

export function contextOptionsFromRgInput(
  input: ZvecGrepRgInput,
): ZvecGrepContextOptions {
  const { queries, options } = parseManagedRgCommand(input.root, input.command);
  return {
    queries: queries.length > 0 ? queries : undefined,
    rg: true,
    rgOptions: options.rgOptions,
    rgPaths: options.rgPaths,
    root: input.root,
    limit: options.limit,
    globs: options.globs,
    insensitiveGlobs: options.insensitiveGlobs,
    fileTypes: options.fileTypes,
    excludedFileTypes: options.excludedFileTypes,
    hidden: options.hidden,
    noIgnore: options.noIgnore,
    ignoreFiles: options.ignoreFiles,
    maxDepth: options.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes,
  };
}

function normalizeSearchFields(
  input: Pick<
    ZvecGrepSearchInput,
    | "query"
    | "queries"
    | "fts"
    | "vector"
    | "globs"
    | "insensitiveGlobs"
    | "fileTypes"
    | "excludedFileTypes"
    | "fuse"
    | "hidden"
    | "noIgnore"
    | "ignoreFiles"
    | "maxDepth"
    | "maxFileSizeBytes"
    | "follow"
    | "embeddingConcurrency"
    | "preferSymbol"
    | "symbolTypes"
    | "modifiedAfter"
    | "modifiedBefore"
    | "limit"
    | "trace"
  >,
) {
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

  return {
    queries: queries.length > 0 ? queries : undefined,
    routes: [
      ...fts.map((query) => ({ mode: "fts" as const, query })),
      ...vector.map((query) => ({ mode: "vector" as const, query })),
    ],
    fuse: input.fuse,
    limit: input.limit,
    trace: input.trace,
    preferSymbol: input.preferSymbol,
    symbolTypes: input.symbolTypes.length > 0 ? input.symbolTypes : undefined,
    globs: normalizePlainStringList(input.globs),
    insensitiveGlobs: normalizePlainStringList(input.insensitiveGlobs),
    fileTypes: normalizePlainStringList(input.fileTypes),
    excludedFileTypes: normalizePlainStringList(input.excludedFileTypes),
    hidden: input.hidden,
    noIgnore: input.noIgnore,
    ignoreFiles: normalizePlainStringList(input.ignoreFiles),
    maxDepth: input.maxDepth,
    maxFileSizeBytes: input.maxFileSizeBytes,
    follow: input.follow,
    embeddingConcurrency: input.embeddingConcurrency,
    modifiedAfter: normalizeModifiedTime(input.modifiedAfter, "modifiedAfter"),
    modifiedBefore: normalizeModifiedTime(
      input.modifiedBefore,
      "modifiedBefore",
    ),
  };
}

export function normalizeQueryList(value: StringListInput): string[] {
  return normalizePlainStringList(value) ?? [];
}

export function normalizePlainStringList(
  value: StringListInput,
): string[] | undefined {
  const items =
    value === undefined ? [] : Array.isArray(value) ? value : [value];
  const normalized = items
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
  return normalized.length > 0 ? normalized : undefined;
}

export function normalizePathFilters(value: StringListInput): string[] {
  if (value === undefined) {
    return [];
  }
  if (Array.isArray(value)) {
    return value.map((item) => item.trim()).filter((item) => item.length > 0);
  }
  return splitPathFilters(value);
}

export function normalizeModifiedTime(
  value: TimeInput,
  option: string,
): number | undefined {
  if (value === undefined) {
    return undefined;
  }
  return typeof value === "number" ? value : parseModifiedTime(value, option);
}
