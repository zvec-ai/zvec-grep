import type { CodeSymbolType, ZvecGrepContextRoute } from "../index.js";
import type { ZvecGrepClientMode } from "../engine/config.js";
import type { McpToolset } from "../mcp/toolset.js";

export type ColorMode = "auto" | "always" | "never";

export type PreviewMode = "none" | "short" | "full";

export type QueryRefreshMode = "background" | "wait" | "off";

export type McpInstallTransport = "stdio" | "http";

export type CliOptions = {
  configAction?: "model-set" | "provider-set";
  defaultModel?: boolean;
  authAction?: "grant" | "status" | "revoke";
  authorizationCapability?: "embedding";
  authorizationScope?: "workspace";
  allowRemote?: boolean;
  serverAction?: "on" | "off" | "status" | "run";
  serverStdio?: boolean;
  mcpToolset?: McpToolset;
  listen?: string;
  serverTokenFile?: string;
  mode?: ZvecGrepClientMode;
  forceDirect?: boolean;
  installTargets?: string[];
  installMcpToolTimeoutSeconds?: number;
  installMcpTokenEnv?: string;
  installMcpTransport?: McpInstallTransport;
  yes?: boolean;
  rg?: boolean;
  rgCompatibilityOptions?: string[];
  rgOptions?: CliRgOptions;
  rgPaths?: string[];
  debug?: boolean;
  trace?: boolean;
  human?: boolean;
  checkReady?: boolean;
  preview?: PreviewMode;
  color?: ColorMode;
  home?: string;
  embedding?: string;
  modelCacheDir?: string;
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
  apiKey?: string;
  endpoint?: string;
  limit?: number;
  hybridQueries?: string[];
  routes?: ZvecGrepContextRoute[];
  fuse?: boolean;
  rebuild?: boolean;
  drop?: boolean;
  force?: boolean;
  resetPaths?: boolean;
  refresh?: QueryRefreshMode;
  preferSymbol?: boolean;
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
  modifiedAfter?: number;
  modifiedBefore?: number;
  symbolTypes?: CodeSymbolType[];
  embeddingConcurrency?: number;
  /** Graph neighborhood / explore traversal depth. */
  depth?: number;
  /** Explore: max files in assembled context. */
  maxFiles?: number;
  /** Explore / neighborhood: disambiguate seed entity id. */
  seedId?: string;
};

export type CliCommand =
  | "query"
  | "explore"
  | "callers"
  | "callees"
  | "impact"
  | "index"
  | "status"
  | "install"
  | "uninstall"
  | "config"
  | "auth"
  | "server"
  | "help"
  | "version";

export type CliRgOptions = {
  patterns?: string[];
  patternFiles?: string[];
  extraArgs?: string[];
  fixedStrings?: boolean;
  ignoreCase?: boolean;
  wordRegexp?: boolean;
  beforeContext?: number;
  afterContext?: number;
  hidden?: boolean;
};

export type ParsedArgs = {
  command: CliCommand;
  options: CliOptions;
  positionals: string[];
  helpTopic?: string;
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
