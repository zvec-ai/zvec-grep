import type { CodeSymbolType, ZvecGrepContextRoute } from "../index.js";

export type ColorMode = "auto" | "always" | "never";

export type PreviewMode = "none" | "short" | "full";

export type CliOptions = {
  help?: boolean;
  version?: boolean;
  index?: boolean;
  disableIndex?: boolean;
  status?: boolean;
  collections?: boolean;
  install?: boolean;
  serve?: boolean;
  mcp?: boolean;
  installTargets?: string[];
  installMcpToolTimeoutSeconds?: number;
  yes?: boolean;
  collection?: string;
  rg?: boolean;
  rgCompatibilityOptions?: string[];
  rgOptions?: CliRgOptions;
  rgPaths?: string[];
  debug?: boolean;
  trace?: boolean;
  human?: boolean;
  preview?: PreviewMode;
  color?: ColorMode;
  home?: string;
  embedding?: string;
  modelCacheDir?: string;
  llamaGpu?: "auto" | "metal" | "vulkan" | "cuda" | false;
  embeddingParallelism?: number;
  apiKey?: string;
  endpoint?: string;
  limit?: number;
  routes?: ZvecGrepContextRoute[];
  rebuild?: boolean;
  force?: boolean;
  resetPaths?: boolean;
  noFallback?: boolean;
  noAutoUpdate?: boolean;
  preferSymbol?: boolean;
  includePaths?: string[];
  excludePaths?: string[];
  modifiedAfter?: number;
  modifiedBefore?: number;
  symbolTypes?: CodeSymbolType[];
  embeddingConcurrency?: number;
};

export type CliRgOptions = {
  patterns?: string[];
  extraArgs?: string[];
  fixedStrings?: boolean;
  ignoreCase?: boolean;
  wordRegexp?: boolean;
  beforeContext?: number;
  afterContext?: number;
  hidden?: boolean;
};

export type ParsedArgs = {
  options: CliOptions;
  positionals: string[];
};

export const DEFAULT_LIMIT = 10;

export const VALID_SYMBOL_TYPES = new Set<CodeSymbolType>([
  "module",
  "class",
  "interface",
  "function",
  "value",
  "alias",
]);
