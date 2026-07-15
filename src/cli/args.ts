import type { CodeSymbolType, ZvecGrepContextRoute } from "../index.js";
import type {
  CliOptions,
  CliRgOptions,
  ColorMode,
  ParsedArgs,
  PreviewMode,
} from "./types.js";
import { VALID_SYMBOL_TYPES } from "./types.js";

const RG_OPTIONS_WITH_VALUE = new Set([
  "--encoding",
  "--engine",
  "--ignore-file",
  "--max-depth",
  "--type",
  "--type-not",
]);

const RG_OPTIONS_WITHOUT_VALUE = new Set([
  "--auto-hybrid-regex",
  "--case-sensitive",
  "--glob-case-insensitive",
  "--multiline",
  "--multiline-dotall",
  "--no-ignore",
  "--no-ignore-dot",
  "--no-ignore-files",
  "--no-ignore-global",
  "--no-ignore-parent",
  "--no-ignore-vcs",
  "--pcre2",
  "--search-zip",
  "--smart-case",
]);

const MANAGED_RG_OUTPUT_OPTIONS = new Set([
  "--count",
  "--count-matches",
  "--files",
  "--files-with-matches",
  "--files-without-match",
  "--json",
  "--only-matching",
  "--replace",
  "--vimgrep",
  "-c",
  "-l",
  "-o",
]);

export function parseArgs(args: readonly string[]): ParsedArgs {
  const options: CliOptions = {};
  const positionals: string[] = [];
  let startIndex = 0;
  if (args[0] === "install") {
    options.install = true;
    startIndex = 1;
  } else if (args[0] === "serve") {
    options.serve = true;
    startIndex = 1;
  }

  for (let index = startIndex; index < args.length; index++) {
    const arg = args[index]!;

    if (arg === "--") {
      positionals.push(...args.slice(index + 1));
      break;
    }

    if (!arg.startsWith("-") || arg === "-") {
      positionals.push(arg);
      continue;
    }

    if (arg === "--help" || arg === "-h") {
      options.help = true;
    } else if (arg === "--version" || arg === "-v") {
      options.version = true;
    } else if (arg === "--index") {
      options.index = true;
    } else if (arg === "--disable-index") {
      options.disableIndex = true;
    } else if (arg === "--status") {
      options.status = true;
    } else if (arg === "--collections") {
      options.collections = true;
    } else if (arg === "--mcp") {
      options.mcp = true;
    } else if (isLongOptionWithValue(arg, "--target")) {
      options.installTargets = appendInstallTargets(
        options.installTargets,
        valueFromLongOption(arg),
      );
    } else if (arg === "--target") {
      options.installTargets = appendInstallTargets(
        options.installTargets,
        readOptionValue(args, ++index, arg),
      );
    } else if (isLongOptionWithValue(arg, "--mcp-tool-timeout")) {
      options.installMcpToolTimeoutSeconds = parsePositiveInteger(
        valueFromLongOption(arg),
        "--mcp-tool-timeout",
      );
    } else if (arg === "--mcp-tool-timeout") {
      options.installMcpToolTimeoutSeconds = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--yes") {
      options.yes = true;
    } else if (arg === "--rg") {
      options.rg = true;
    } else if (arg === "--debug") {
      options.debug = true;
    } else if (arg === "--trace") {
      options.trace = true;
    } else if (arg === "--human") {
      options.human = true;
    } else if (isLongOptionWithValue(arg, "--preview")) {
      options.preview = parsePreviewMode(valueFromLongOption(arg));
    } else if (arg === "--preview") {
      options.preview = parsePreviewMode(readOptionValue(args, ++index, arg));
    } else if (arg === "--json") {
      throw new Error(
        "--json has been removed; use the default agent markdown output or --human",
      );
    } else if (arg === "--no-color") {
      options.color = "never";
    } else if (arg === "--rebuild") {
      options.rebuild = true;
    } else if (arg === "--force") {
      options.force = true;
    } else if (arg === "--reset-paths") {
      options.resetPaths = true;
    } else if (arg === "--no-fallback") {
      options.noFallback = true;
    } else if (arg === "--no-auto-update") {
      options.noAutoUpdate = true;
    } else if (arg === "--prefer-symbol") {
      options.preferSymbol = true;
    } else if (arg === "--collection") {
      options.collection = readOptionValue(args, ++index, arg);
    } else if (arg === "--home") {
      options.home = readOptionValue(args, ++index, arg);
    } else if (arg === "--embedding") {
      options.embedding = readOptionValue(args, ++index, arg);
    } else if (arg === "--model-cache") {
      options.modelCacheDir = readOptionValue(args, ++index, arg);
    } else if (arg === "--gpu") {
      options.llamaGpu = "auto";
    } else if (arg === "--no-gpu") {
      options.llamaGpu = false;
    } else if (arg === "--llama-gpu") {
      options.llamaGpu = parseLlamaGpu(readOptionValue(args, ++index, arg));
    } else if (arg === "--embedding-parallelism") {
      options.embeddingParallelism = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--api-key") {
      options.apiKey = readOptionValue(args, ++index, arg);
    } else if (arg === "--endpoint") {
      options.endpoint = readOptionValue(args, ++index, arg);
    } else if (arg === "--limit") {
      options.limit = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--embedding-concurrency") {
      options.embeddingConcurrency = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--fts") {
      const routeValues = readRouteOptionValues(args, index + 1, arg);
      options.routes = appendRoutes(options.routes, "fts", routeValues.values);
      index = routeValues.nextIndex - 1;
    } else if (arg === "--vector") {
      const routeValues = readRouteOptionValues(args, index + 1, arg);
      options.routes = appendRoutes(
        options.routes,
        "vector",
        routeValues.values,
      );
      index = routeValues.nextIndex - 1;
    } else if (arg === "--color") {
      options.color = parseColorMode(readOptionValue(args, ++index, arg));
    } else if (isLongOptionWithValue(arg, "--include")) {
      options.includePaths = appendPathFilters(
        options.includePaths,
        valueFromLongOption(arg),
      );
    } else if (arg === "--include") {
      options.includePaths = appendPathFilters(
        options.includePaths,
        readOptionValue(args, ++index, arg),
      );
    } else if (isLongOptionWithValue(arg, "--exclude")) {
      options.excludePaths = appendPathFilters(
        options.excludePaths,
        valueFromLongOption(arg),
      );
    } else if (arg === "--exclude") {
      options.excludePaths = appendPathFilters(
        options.excludePaths,
        readOptionValue(args, ++index, arg),
      );
    } else if (arg === "--ignore-case") {
      options.rgOptions = {
        ...(options.rgOptions ?? {}),
        ignoreCase: true,
      };
      markRgCompatibilityOption(options, arg);
    } else if (arg === "--word-regexp") {
      options.rgOptions = {
        ...(options.rgOptions ?? {}),
        wordRegexp: true,
      };
      markRgCompatibilityOption(options, arg);
    } else if (arg === "--fixed-strings") {
      options.rgOptions = {
        ...(options.rgOptions ?? {}),
        fixedStrings: true,
      };
      markRgCompatibilityOption(options, arg);
    } else if (arg === "--hidden") {
      options.rgOptions = {
        ...(options.rgOptions ?? {}),
        hidden: true,
      };
      markRgCompatibilityOption(options, arg);
    } else if (isRgFlagWithValue(arg)) {
      const option = optionNameFromLong(arg);
      options.rgOptions = appendRgExtraArgs(options.rgOptions, [
        option,
        valueFromLongOption(arg),
      ]);
      markRgCompatibilityOption(options, option);
    } else if (RG_OPTIONS_WITH_VALUE.has(arg)) {
      options.rgOptions = appendRgExtraArgs(options.rgOptions, [
        arg,
        readOptionValue(args, ++index, arg),
      ]);
      markRgCompatibilityOption(options, arg);
    } else if (RG_OPTIONS_WITHOUT_VALUE.has(arg)) {
      options.rgOptions = appendRgExtraArgs(options.rgOptions, [arg]);
      markRgCompatibilityOption(options, arg);
    } else if (
      arg === "--recursive" ||
      arg === "--line-number" ||
      arg === "--with-filename"
    ) {
      markRgCompatibilityOption(options, arg);
    } else if (isLongOptionWithValue(arg, "--regexp")) {
      options.rgOptions = appendRgPattern(
        options.rgOptions,
        valueFromLongOption(arg),
      );
      markRgCompatibilityOption(options, "--regexp");
    } else if (arg === "--regexp") {
      options.rgOptions = appendRgPattern(
        options.rgOptions,
        readOptionValue(args, ++index, arg),
      );
      markRgCompatibilityOption(options, arg);
    } else if (isLongOptionWithValue(arg, "--context")) {
      options.rgOptions = setRgContext(
        options.rgOptions,
        "both",
        valueFromLongOption(arg),
        "--context",
      );
      markRgCompatibilityOption(options, "--context");
    } else if (arg === "--context") {
      options.rgOptions = setRgContext(
        options.rgOptions,
        "both",
        readOptionValue(args, ++index, arg),
        arg,
      );
      markRgCompatibilityOption(options, arg);
    } else if (isLongOptionWithValue(arg, "--before-context")) {
      options.rgOptions = setRgContext(
        options.rgOptions,
        "before",
        valueFromLongOption(arg),
        "--before-context",
      );
      markRgCompatibilityOption(options, "--before-context");
    } else if (arg === "--before-context") {
      options.rgOptions = setRgContext(
        options.rgOptions,
        "before",
        readOptionValue(args, ++index, arg),
        arg,
      );
      markRgCompatibilityOption(options, arg);
    } else if (isLongOptionWithValue(arg, "--after-context")) {
      options.rgOptions = setRgContext(
        options.rgOptions,
        "after",
        valueFromLongOption(arg),
        "--after-context",
      );
      markRgCompatibilityOption(options, "--after-context");
    } else if (arg === "--after-context") {
      options.rgOptions = setRgContext(
        options.rgOptions,
        "after",
        readOptionValue(args, ++index, arg),
        arg,
      );
      markRgCompatibilityOption(options, arg);
    } else if (isLongOptionWithValue(arg, "--glob")) {
      appendRgGlobFilter(options, valueFromLongOption(arg));
      markRgCompatibilityOption(options, "--glob");
    } else if (arg === "--glob") {
      appendRgGlobFilter(options, readOptionValue(args, ++index, arg));
      markRgCompatibilityOption(options, arg);
    } else if (arg === "-g") {
      appendRgGlobFilter(options, readOptionValue(args, ++index, arg));
      markRgCompatibilityOption(options, arg);
    } else if (isManagedRgOutputOption(arg)) {
      throw new Error(
        `${arg} changes rg output and cannot be used with managed --rg`,
      );
    } else if (isShortRgOptionGroup(arg)) {
      index = parseShortRgOptionGroup(args, index, options);
    } else if (arg === "--modified-after") {
      options.modifiedAfter = parseModifiedTime(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--modified-before") {
      options.modifiedBefore = parseModifiedTime(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--symbol-type") {
      options.symbolTypes = [
        ...(options.symbolTypes ?? []),
        parseSymbolType(readOptionValue(args, ++index, arg)),
      ];
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }

  validateCliShape(options);
  return { options, positionals };
}

function validateCliShape(options: CliOptions): void {
  const hasUtilityCommand =
    options.index ||
    options.disableIndex ||
    options.status ||
    options.collections ||
    options.install ||
    options.serve;

  if (options.index && options.collections) {
    throw new Error("--index and --collections cannot be used together");
  }

  if (
    options.install &&
    (options.index ||
      options.disableIndex ||
      options.status ||
      options.collections)
  ) {
    throw new Error(
      "zg install cannot be combined with index, status, or collections commands",
    );
  }

  if (
    options.serve &&
    (options.index ||
      options.disableIndex ||
      options.status ||
      options.collections)
  ) {
    throw new Error(
      "zg serve cannot be combined with index, status, or collections commands",
    );
  }

  if (options.serve && options.install) {
    throw new Error("zg serve cannot be combined with install");
  }

  if (options.serve && !options.mcp) {
    throw new Error("zg serve currently requires --mcp");
  }

  if (options.mcp && !options.serve) {
    throw new Error("--mcp can only be used with zg serve");
  }

  if (options.disableIndex && options.collections) {
    throw new Error(
      "--disable-index and --collections cannot be used together",
    );
  }

  if (options.disableIndex && options.index) {
    throw new Error("--disable-index and --index cannot be used together");
  }

  if (options.disableIndex && options.status) {
    throw new Error("--disable-index and --status cannot be used together");
  }

  if (options.status && options.collections) {
    throw new Error("--status and --collections cannot be used together");
  }

  if (options.status && options.index) {
    throw new Error("--status and --index cannot be used together");
  }

  if (options.status && options.collection) {
    throw new Error(
      "--status does not accept --collection; use --collections info <name>",
    );
  }

  if (options.index && options.collection) {
    throw new Error("--index does not accept --collection");
  }

  if (options.disableIndex && options.collection) {
    throw new Error("--disable-index does not accept --collection");
  }

  if (options.rg && options.collection) {
    throw new Error("--rg does not accept --collection");
  }

  if (options.collections && options.collection) {
    throw new Error("--collections and --collection cannot be used together");
  }

  if (options.install && options.collection) {
    throw new Error("zg install does not accept --collection");
  }

  if (options.installMcpToolTimeoutSeconds !== undefined && !options.install) {
    throw new Error("--mcp-tool-timeout can only be used with zg install");
  }

  if (options.serve && options.collection) {
    throw new Error("zg serve does not accept --collection");
  }

  if (hasUtilityCommand && hasExplicitRoutes(options)) {
    throw new Error("--fts and --vector can only be used with query commands");
  }

  if (hasUtilityCommand && options.rg) {
    throw new Error("--rg can only be used with query commands");
  }

  if (hasUtilityCommand && options.preview) {
    throw new Error("--preview can only be used with query commands");
  }

  if (hasUtilityCommand && options.noAutoUpdate) {
    throw new Error("--no-auto-update can only be used with query commands");
  }

  if (options.rg && hasExplicitRoutes(options)) {
    throw new Error("--rg cannot be combined with --fts or --vector");
  }

  if (options.rg && options.preview) {
    throw new Error(
      "--preview is not supported with --rg; use -A/-B/-C for rg context",
    );
  }

  if (options.rg && options.noFallback) {
    throw new Error("--rg cannot be combined with --no-fallback");
  }

  if (options.rg && options.trace) {
    throw new Error("--rg cannot be combined with --trace");
  }

  if (options.rg && (options.preferSymbol || options.symbolTypes?.length)) {
    throw new Error("--rg cannot be combined with indexed symbol options");
  }

  if (options.resetPaths && !options.index && !options.collections) {
    throw new Error(
      "--reset-paths can only be used with --index or --collections index",
    );
  }

  if (!options.rg && (options.rgCompatibilityOptions?.length ?? 0) > 0) {
    const [option] = options.rgCompatibilityOptions!;
    throw new Error(`${option} can only be used with --rg`);
  }
}

function readOptionValue(
  args: readonly string[],
  index: number,
  option: string,
): string {
  const value = args[index];
  if (value === undefined || value.length === 0) {
    throw new Error(`${option} requires a value`);
  }

  return value;
}

function isLongOptionWithValue(arg: string, option: string): boolean {
  return arg.startsWith(`${option}=`);
}

function valueFromLongOption(arg: string): string {
  const separator = arg.indexOf("=");
  return separator >= 0 ? arg.slice(separator + 1) : "";
}

function optionNameFromLong(arg: string): string {
  const separator = arg.indexOf("=");
  return separator >= 0 ? arg.slice(0, separator) : arg;
}

function isRgFlagWithValue(arg: string): boolean {
  const option = optionNameFromLong(arg);
  return arg.includes("=") && RG_OPTIONS_WITH_VALUE.has(option);
}

function isManagedRgOutputOption(arg: string): boolean {
  const option = optionNameFromLong(arg);
  return MANAGED_RG_OUTPUT_OPTIONS.has(option);
}

function isShortRgOptionGroup(arg: string): boolean {
  return /^-[A-Za-z].*$/.test(arg) && arg !== "-h" && arg !== "-v";
}

function parseShortRgOptionGroup(
  args: readonly string[],
  index: number,
  options: CliOptions,
): number {
  const arg = args[index]!;
  for (let offset = 1; offset < arg.length; offset++) {
    const option = arg[offset]!;
    const flag = `-${option}`;

    switch (option) {
      case "n":
      case "H":
        markRgCompatibilityOption(options, flag);
        break;
      case "F":
        options.rgOptions = {
          ...(options.rgOptions ?? {}),
          fixedStrings: true,
        };
        markRgCompatibilityOption(options, flag);
        break;
      case "i":
        options.rgOptions = {
          ...(options.rgOptions ?? {}),
          ignoreCase: true,
        };
        markRgCompatibilityOption(options, flag);
        break;
      case "w":
        options.rgOptions = {
          ...(options.rgOptions ?? {}),
          wordRegexp: true,
        };
        markRgCompatibilityOption(options, flag);
        break;
      case "P":
      case "S":
      case "s":
      case "u":
      case "U":
      case "z":
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [flag]);
        markRgCompatibilityOption(options, flag);
        break;
      case "e": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        options.rgOptions = appendRgPattern(options.rgOptions, value.value);
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "g": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        appendRgGlobFilter(options, value.value);
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "t":
      case "T":
      case "E": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [
          flag,
          value.value,
        ]);
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "A":
      case "B":
      case "C": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        const direction =
          option === "A" ? "after" : option === "B" ? "before" : "both";
        options.rgOptions = setRgContext(
          options.rgOptions,
          direction,
          value.value,
          flag,
        );
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      default:
        if (isManagedRgOutputOption(flag)) {
          throw new Error(
            `${flag} changes rg output and cannot be used with managed --rg`,
          );
        }
        throw new Error(`Unsupported --rg option: ${flag}`);
    }
  }

  return index;
}

function readShortOptionValue(
  args: readonly string[],
  index: number,
  offset: number,
  arg: string,
  option: string,
): { value: string; nextIndex: number } {
  const inline = arg.slice(offset + 1);
  if (inline.length > 0) {
    return {
      value: inline,
      nextIndex: index,
    };
  }

  return {
    value: readOptionValue(args, index + 1, option),
    nextIndex: index + 1,
  };
}

function markRgCompatibilityOption(options: CliOptions, option: string): void {
  options.rgCompatibilityOptions = [
    ...(options.rgCompatibilityOptions ?? []),
    option,
  ];
}

function appendRgPattern(
  existing: CliRgOptions | undefined,
  value: string,
): CliRgOptions {
  return {
    ...(existing ?? {}),
    patterns: [...(existing?.patterns ?? []), value],
  };
}

function appendInstallTargets(
  existing: string[] | undefined,
  value: string,
): string[] {
  const targets = value
    .split(/[,\s]+/)
    .map((target) => target.trim())
    .filter((target) => target.length > 0);

  return [...(existing ?? []), ...targets];
}

function setRgContext(
  existing: CliRgOptions | undefined,
  direction: "before" | "after" | "both",
  value: string,
  option: string,
): CliRgOptions {
  const parsed = parseNonNegativeInteger(value, option);
  return {
    ...(existing ?? {}),
    beforeContext:
      direction === "before" || direction === "both"
        ? parsed
        : existing?.beforeContext,
    afterContext:
      direction === "after" || direction === "both"
        ? parsed
        : existing?.afterContext,
  };
}

function appendRgExtraArgs(
  existing: CliRgOptions | undefined,
  args: readonly string[],
): CliRgOptions {
  return {
    ...(existing ?? {}),
    extraArgs: [...(existing?.extraArgs ?? []), ...args],
  };
}

function appendRgGlobFilter(options: CliOptions, value: string): void {
  if (value.startsWith("!")) {
    options.excludePaths = appendPathFilters(
      options.excludePaths,
      value.slice(1),
    );
    return;
  }

  options.includePaths = appendPathFilters(options.includePaths, value);
}

function readRouteOptionValues(
  args: readonly string[],
  index: number,
  option: string,
): { values: string[]; nextIndex: number } {
  const values: string[] = [];
  let nextIndex = index;

  while (nextIndex < args.length) {
    const value = args[nextIndex]!;
    if (value.startsWith("-") && value !== "-") {
      break;
    }

    if (value.trim().length > 0) {
      values.push(value);
    }
    nextIndex++;
  }

  if (values.length === 0) {
    throw new Error(`${option} requires at least one query`);
  }

  return { values, nextIndex };
}

function parsePositiveInteger(value: string, option: string): number {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isInteger(parsed) || parsed < 1) {
    throw new Error(`${option} requires a positive integer`);
  }

  return parsed;
}

function parseNonNegativeInteger(value: string, option: string): number {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${option} requires a non-negative integer`);
  }

  return parsed;
}

function parseColorMode(value: string): ColorMode {
  if (value === "auto" || value === "always" || value === "never") {
    return value;
  }

  throw new Error(`Unsupported color mode: ${value}`);
}

function parsePreviewMode(value: string): PreviewMode {
  if (value === "none" || value === "short" || value === "full") {
    return value;
  }

  throw new Error(`Unsupported preview mode: ${value}`);
}

export function parseLlamaGpu(
  value: string,
): "auto" | "metal" | "vulkan" | "cuda" | false {
  const normalized = value.trim().toLowerCase();
  if (
    normalized === "auto" ||
    normalized === "metal" ||
    normalized === "vulkan" ||
    normalized === "cuda"
  ) {
    return normalized;
  }

  if (
    ["false", "off", "none", "disable", "disabled", "0"].includes(normalized)
  ) {
    return false;
  }

  throw new Error(`Unsupported llama GPU mode: ${value}`);
}

function hasExplicitRoutes(options: CliOptions): boolean {
  return (options.routes?.length ?? 0) > 0;
}

function appendRoutes(
  existing: ZvecGrepContextRoute[] | undefined,
  mode: ZvecGrepContextRoute["mode"],
  queries: readonly string[],
): ZvecGrepContextRoute[] {
  return [...(existing ?? []), ...queries.map((query) => ({ mode, query }))];
}

function parseSymbolType(value: string): CodeSymbolType {
  if (VALID_SYMBOL_TYPES.has(value as CodeSymbolType)) {
    return value as CodeSymbolType;
  }

  throw new Error(`Unsupported symbol type: ${value}`);
}

export function splitPathFilters(value: string): string[] {
  return value
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

function appendPathFilters(
  existing: string[] | undefined,
  value: string,
): string[] {
  return [...(existing ?? []), ...splitPathFilters(value)];
}

export function parseModifiedTime(value: string, option: string): number {
  if (/^\d+$/.test(value)) {
    const parsed = Number.parseInt(value, 10);
    if (Number.isSafeInteger(parsed) && parsed >= 0) {
      return parsed;
    }
  }

  const dateOnly = value.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (dateOnly) {
    const year = Number.parseInt(dateOnly[1]!, 10);
    const month = Number.parseInt(dateOnly[2]!, 10);
    const day = Number.parseInt(dateOnly[3]!, 10);
    const date = new Date(year, month - 1, day);
    if (
      date.getFullYear() === year &&
      date.getMonth() === month - 1 &&
      date.getDate() === day
    ) {
      return date.getTime();
    }
  }

  const parsed = Date.parse(value);
  if (Number.isFinite(parsed)) {
    return parsed;
  }

  throw new Error(
    `${option} requires an epoch millisecond value or a parseable date`,
  );
}
