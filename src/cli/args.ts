import type { CodeSymbolType, ZvecGrepContextRoute } from "../index.js";
import type {
  CliCommand,
  CliOptions,
  CliRgOptions,
  ColorMode,
  McpInstallTransport,
  ParsedArgs,
  PreviewMode,
  QueryRefreshMode,
} from "./types.js";
import { VALID_SYMBOL_TYPES } from "./types.js";
import { parseMcpToolset } from "../mcp/toolset.js";

const RG_OPTIONS_WITH_VALUE = new Set([
  "--dfa-size-limit",
  "--encoding",
  "--engine",
  "--max-columns",
  "--max-count",
  "--regex-size-limit",
  "--threads",
]);

const RG_OPTIONS_WITHOUT_VALUE = new Set([
  "--auto-hybrid-regex",
  "--case-sensitive",
  "--binary",
  "--crlf",
  "--invert-match",
  "--line-regexp",
  "--mmap",
  "--multiline",
  "--multiline-dotall",
  "--no-crlf",
  "--no-fixed-strings",
  "--no-ignore",
  "--no-ignore-dot",
  "--no-ignore-files",
  "--no-ignore-global",
  "--no-ignore-parent",
  "--no-ignore-vcs",
  "--no-config",
  "--no-mmap",
  "--no-multiline",
  "--no-search-zip",
  "--pcre2",
  "--one-file-system",
  "--search-zip",
  "--smart-case",
  "--stop-on-nonmatch",
  "--text",
  "--unicode",
  "--no-unicode",
  "--glob-case-insensitive",
]);

const MANAGED_RG_OUTPUT_OPTIONS = new Set([
  "--count",
  "--count-matches",
  "--files",
  "--files-with-matches",
  "--files-without-match",
  "--column",
  "--byte-offset",
  "--no-column",
  "--colors",
  "--context-separator",
  "--field-context-separator",
  "--field-match-separator",
  "--json",
  "--heading",
  "--no-heading",
  "--no-filename",
  "--no-line-number",
  "--only-matching",
  "--passthru",
  "--path-separator",
  "--quiet",
  "--pretty",
  "--replace",
  "--stats",
  "--trim",
  "--vimgrep",
  "-c",
  "-b",
  "-I",
  "-l",
  "-N",
  "-o",
  "-p",
  "-q",
  "-r",
]);

export function parseArgs(args: readonly string[]): ParsedArgs {
  const commandInput = parseCommand(args);
  if (commandInput.command === "help" || commandInput.command === "version") {
    return {
      command: commandInput.command,
      options: {},
      positionals: [],
      ...(commandInput.helpTopic ? { helpTopic: commandInput.helpTopic } : {}),
    };
  }

  const options: CliOptions = {};
  const positionals: string[] = [];
  let startIndex = 1;
  if (commandInput.command === "config") {
    if (args[1] === "model" && args[2] === "set") {
      options.configAction = "model-set";
      startIndex = 3;
    } else if (args[1] === "provider" && args[2] === "set") {
      options.configAction = "provider-set";
      startIndex = 3;
    }
  } else if (commandInput.command === "auth") {
    if (args[1] === "grant" || args[1] === "status" || args[1] === "revoke") {
      options.authAction = args[1];
      startIndex = 2;
    }
  } else if (commandInput.command === "server") {
    if (
      args[1] === "on" ||
      args[1] === "off" ||
      args[1] === "status" ||
      args[1] === "run"
    ) {
      options.serverAction = args[1];
      startIndex = 2;
    }
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
      return {
        command: "help",
        options: {},
        positionals: [],
        helpTopic: commandInput.command,
      };
    } else if (arg === "--version") {
      throw new Error(`${arg} must be used without a command`);
    } else if (isLongOptionWithValue(arg, "--mode")) {
      options.mode = parseClientMode(valueFromLongOption(arg));
    } else if (arg === "--mode") {
      options.mode = parseClientMode(readOptionValue(args, ++index, arg));
    } else if (arg === "--force-direct") {
      options.forceDirect = true;
    } else if (arg === "--stdio") {
      options.serverStdio = true;
    } else if (isLongOptionWithValue(arg, "--mcp-toolset")) {
      options.mcpToolset = parseMcpToolset(valueFromLongOption(arg));
    } else if (arg === "--mcp-toolset") {
      options.mcpToolset = parseMcpToolset(readOptionValue(args, ++index, arg));
    } else if (isLongOptionWithValue(arg, "--listen")) {
      options.listen = valueFromLongOption(arg);
    } else if (arg === "--listen") {
      options.listen = readOptionValue(args, ++index, arg);
    } else if (isLongOptionWithValue(arg, "--token-file")) {
      options.serverTokenFile = valueFromLongOption(arg);
    } else if (arg === "--token-file") {
      options.serverTokenFile = readOptionValue(args, ++index, arg);
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
    } else if (isLongOptionWithValue(arg, "--mcp-token-env")) {
      options.installMcpTokenEnv = parseEnvironmentVariable(
        valueFromLongOption(arg),
        "--mcp-token-env",
      );
    } else if (arg === "--mcp-token-env") {
      options.installMcpTokenEnv = parseEnvironmentVariable(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--mcp-transport")) {
      options.installMcpTransport = parseMcpInstallTransport(
        valueFromLongOption(arg),
      );
    } else if (arg === "--mcp-transport") {
      options.installMcpTransport = parseMcpInstallTransport(
        readOptionValue(args, ++index, arg),
      );
    } else if (arg === "--yes") {
      options.yes = true;
    } else if (isLongOptionWithValue(arg, "--allow-remote")) {
      throw allowRemoteValueError();
    } else if (arg === "--allow-remote") {
      const legacyScope = args[index + 1];
      if (legacyScope === "once" || legacyScope === "workspace") {
        throw allowRemoteValueError();
      }
      options.allowRemote = true;
    } else if (isLongOptionWithValue(arg, "--capability")) {
      options.authorizationCapability = parseAuthorizationCapability(
        valueFromLongOption(arg),
      );
    } else if (arg === "--capability") {
      options.authorizationCapability = parseAuthorizationCapability(
        readOptionValue(args, ++index, arg),
      );
    } else if (isLongOptionWithValue(arg, "--scope")) {
      options.authorizationScope = parseAuthorizationScope(
        valueFromLongOption(arg),
      );
    } else if (arg === "--scope") {
      options.authorizationScope = parseAuthorizationScope(
        readOptionValue(args, ++index, arg),
      );
    } else if (arg === "--rg") {
      options.rg = true;
    } else if (arg === "--debug") {
      options.debug = true;
    } else if (arg === "--trace") {
      options.trace = true;
    } else if (arg === "--human") {
      options.human = true;
    } else if (arg === "--check-ready") {
      options.checkReady = true;
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
    } else if (arg === "--drop") {
      options.drop = true;
    } else if (arg === "--force") {
      options.force = true;
    } else if (arg === "--reset-paths") {
      options.resetPaths = true;
    } else if (arg === "--default") {
      options.defaultModel = true;
    } else if (isLongOptionWithValue(arg, "--refresh")) {
      options.refresh = parseQueryRefreshMode(valueFromLongOption(arg));
    } else if (arg === "--refresh") {
      options.refresh = parseQueryRefreshMode(
        readOptionValue(args, ++index, arg),
      );
    } else if (arg === "--prefer-symbol") {
      options.preferSymbol = true;
    } else if (arg === "--home") {
      options.home = readOptionValue(args, ++index, arg);
    } else if (arg === "--embedding") {
      options.embedding = readOptionValue(args, ++index, arg);
    } else if (arg === "--model-cache") {
      options.modelCacheDir = readOptionValue(args, ++index, arg);
    } else if (arg === "--device") {
      options.device = parseDevice(readOptionValue(args, ++index, arg));
    } else if (arg === "--api-key") {
      options.apiKey = readOptionValue(args, ++index, arg);
    } else if (arg === "--endpoint") {
      options.endpoint = readOptionValue(args, ++index, arg);
    } else if (arg === "--limit") {
      options.limit = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--depth") {
      options.depth = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--max-files") {
      options.maxFiles = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--seed-id") {
      options.seedId = readOptionValue(args, ++index, arg);
    } else if (arg === "--embedding-concurrency") {
      options.embeddingConcurrency = parsePositiveInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--hybrid")) {
      options.hybridQueries = appendQuery(
        options.hybridQueries,
        valueFromLongOption(arg),
        "--hybrid",
      );
    } else if (arg === "--hybrid") {
      options.hybridQueries = appendQuery(
        options.hybridQueries,
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--fts")) {
      options.routes = appendRoute(
        options.routes,
        "fts",
        valueFromLongOption(arg),
        "--fts",
      );
    } else if (arg === "--fts") {
      options.routes = appendRoute(
        options.routes,
        "fts",
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--vector")) {
      options.routes = appendRoute(
        options.routes,
        "vector",
        valueFromLongOption(arg),
        "--vector",
      );
    } else if (arg === "--vector") {
      options.routes = appendRoute(
        options.routes,
        "vector",
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--fuse") {
      options.fuse = true;
    } else if (arg === "--color") {
      options.color = parseColorMode(readOptionValue(args, ++index, arg));
    } else if (isLongOptionWithValue(arg, "--glob")) {
      options.globs = appendValue(
        options.globs,
        valueFromLongOption(arg),
        "--glob",
      );
    } else if (arg === "--glob" || arg === "-g") {
      options.globs = appendValue(
        options.globs,
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--iglob")) {
      options.insensitiveGlobs = appendValue(
        options.insensitiveGlobs,
        valueFromLongOption(arg),
        "--iglob",
      );
    } else if (arg === "--iglob") {
      options.insensitiveGlobs = appendValue(
        options.insensitiveGlobs,
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--type")) {
      options.fileTypes = appendValue(
        options.fileTypes,
        valueFromLongOption(arg),
        "--type",
      );
    } else if (arg === "--type" || arg === "-t") {
      options.fileTypes = appendValue(
        options.fileTypes,
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--type-not")) {
      options.excludedFileTypes = appendValue(
        options.excludedFileTypes,
        valueFromLongOption(arg),
        "--type-not",
      );
    } else if (arg === "--type-not" || arg === "-T") {
      options.excludedFileTypes = appendValue(
        options.excludedFileTypes,
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--hidden") {
      options.hidden = true;
    } else if (arg === "--no-ignore") {
      options.noIgnore = true;
    } else if (isLongOptionWithValue(arg, "--ignore-file")) {
      options.ignoreFiles = appendValue(
        options.ignoreFiles,
        valueFromLongOption(arg),
        "--ignore-file",
      );
    } else if (arg === "--ignore-file") {
      options.ignoreFiles = appendValue(
        options.ignoreFiles,
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--max-depth")) {
      options.maxDepth = parseNonNegativeInteger(
        valueFromLongOption(arg),
        "--max-depth",
      );
    } else if (arg === "--max-depth") {
      options.maxDepth = parseNonNegativeInteger(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (isLongOptionWithValue(arg, "--max-filesize")) {
      options.maxFileSizeBytes = parseByteSize(
        valueFromLongOption(arg),
        "--max-filesize",
      );
    } else if (arg === "--max-filesize") {
      options.maxFileSizeBytes = parseByteSize(
        readOptionValue(args, ++index, arg),
        arg,
      );
    } else if (arg === "--follow" || arg === "-L") {
      options.follow = true;
    } else if (arg === "--ignore-case") {
      options.rgOptions = appendRgExtraArgs(options.rgOptions, [arg]);
      markRgCompatibilityOption(options, arg);
    } else if (arg === "--word-regexp") {
      options.rgOptions = appendRgExtraArgs(options.rgOptions, [arg]);
      markRgCompatibilityOption(options, arg);
    } else if (arg === "--fixed-strings") {
      options.rgOptions = appendRgExtraArgs(options.rgOptions, [arg]);
      markRgCompatibilityOption(options, arg);
    } else if (isLongOptionWithValue(arg, "--file")) {
      options.rgOptions = appendRgPatternFile(
        options.rgOptions,
        valueFromLongOption(arg),
        "--file",
      );
      markRgCompatibilityOption(options, "--file");
    } else if (arg === "--file") {
      options.rgOptions = appendRgPatternFile(
        options.rgOptions,
        readOptionValue(args, ++index, arg),
        arg,
      );
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

  validateCliShape(commandInput.command, options, positionals);
  return { command: commandInput.command, options, positionals };
}

function parseCommand(args: readonly string[]): {
  command: CliCommand;
  helpTopic?: string;
} {
  const [first, ...rest] = args;
  if (
    first === undefined ||
    first === "help" ||
    first === "-h" ||
    first === "--help"
  ) {
    if (first !== "help" && rest.length > 0) {
      throw new Error(`${first} does not accept arguments`);
    }
    if (first === "help" && rest.length > 1) {
      throw new Error("zg help accepts at most one command or topic");
    }
    return {
      command: "help",
      ...(first === "help" && rest[0] ? { helpTopic: rest[0] } : {}),
    };
  }

  if (first === "version" || first === "-v" || first === "--version") {
    const acceptsVersionFlag =
      first === "version" &&
      rest.length === 1 &&
      (rest[0] === "-v" || rest[0] === "--version");
    if (rest.length > 0 && !acceptsVersionFlag) {
      throw new Error(`${first} does not accept arguments`);
    }
    return { command: "version" };
  }

  if (
    first === "query" ||
    first === "explore" ||
    first === "callers" ||
    first === "callees" ||
    first === "impact" ||
    first === "index" ||
    first === "status" ||
    first === "install" ||
    first === "uninstall" ||
    first === "config" ||
    first === "auth" ||
    first === "server"
  ) {
    return { command: first };
  }

  if (first === "serve") {
    throw new Error(
      "zg serve has been removed; use zg server on and Streamable HTTP MCP",
    );
  }

  throw new Error(`Unknown command: ${first}`);
}

function validateCliShape(
  command: CliCommand,
  options: CliOptions,
  positionals: readonly string[],
): void {
  if (command === "auth") {
    if (!options.authAction) {
      throw new Error("zg auth requires grant, status, or revoke");
    }
    if (positionals.length > 1) {
      throw new Error(`zg auth ${options.authAction} accepts at most one root`);
    }
    if (
      options.authAction !== "grant" &&
      (options.authorizationCapability || options.authorizationScope)
    ) {
      throw new Error(
        `--capability and --scope can only be used with zg auth grant`,
      );
    }
  } else if (options.authorizationCapability || options.authorizationScope) {
    throw new Error("--capability and --scope can only be used with zg auth");
  }

  if (options.allowRemote && command !== "query" && command !== "index") {
    throw new Error(
      "--allow-remote can only be used with query or index commands",
    );
  }
  const graphCommand =
    command === "explore" ||
    command === "callers" ||
    command === "callees" ||
    command === "impact";

  const queryOnly = firstEnabledOption([
    [options.rg, "--rg"],
    [options.hybridQueries?.length, "--hybrid"],
    [options.routes?.length, "--fts/--vector"],
    [options.fuse, "--fuse"],
    [options.trace, "--trace"],
    [options.human, "--human"],
    [options.preview, "--preview"],
    [graphCommand ? undefined : options.limit, "--limit"],
    [options.refresh, "--refresh"],
    [options.preferSymbol, "--prefer-symbol"],
    [options.symbolTypes?.length, "--symbol-type"],
    [options.modifiedAfter, "--modified-after"],
    [options.modifiedBefore, "--modified-before"],
    [options.rgCompatibilityOptions?.length, "ripgrep options"],
  ]);
  if (command !== "query" && queryOnly) {
    throw new Error(`${queryOnly} can only be used with zg query`);
  }
  if (options.debug && command !== "query" && command !== "index") {
    throw new Error("--debug can only be used with zg query or zg index");
  }

  const graphOnly = firstEnabledOption([
    [options.depth, "--depth"],
    [options.maxFiles, "--max-files"],
    [options.seedId, "--seed-id"],
  ]);
  if (graphOnly && !graphCommand) {
    throw new Error(
      `${graphOnly} can only be used with explore/callers/callees/impact`,
    );
  }
  if (options.maxFiles !== undefined && command !== "explore") {
    throw new Error("--max-files can only be used with zg explore");
  }
  if (graphCommand && positionals.length !== 1) {
    throw new Error(`zg ${command} requires exactly one query`);
  }

  const sharedSelection = firstEnabledOption([
    [options.globs?.length, "--glob"],
    [options.insensitiveGlobs?.length, "--iglob"],
    [options.fileTypes?.length, "--type"],
    [options.excludedFileTypes?.length, "--type-not"],
  ]);
  if (sharedSelection && command !== "query" && command !== "index") {
    throw new Error(
      `${sharedSelection} can only be used with query or index commands`,
    );
  }

  const discoveryOption = firstEnabledOption([
    [options.hidden, "--hidden"],
    [options.noIgnore, "--no-ignore"],
    [options.ignoreFiles?.length, "--ignore-file"],
    [options.maxDepth, "--max-depth"],
    [options.maxFileSizeBytes, "--max-filesize"],
    [options.follow, "--follow"],
  ]);
  if (
    discoveryOption &&
    command !== "index" &&
    !(command === "query" && options.rg)
  ) {
    throw new Error(
      `${discoveryOption} can only be used with index commands or zg query --rg`,
    );
  }

  if (command === "config") {
    if (!options.configAction) {
      throw new Error("zg config requires provider set or model set");
    }
    const unsupported = firstEnabledOption([
      [options.installTargets?.length, "--target"],
      [options.installMcpToolTimeoutSeconds, "--mcp-tool-timeout"],
      [options.installMcpTokenEnv, "--mcp-token-env"],
      [options.yes, "--yes"],
      [options.rg, "--rg"],
      [options.home, "--home"],
      [options.embedding, "--embedding"],
      [
        options.configAction === "model-set" ? options.apiKey : undefined,
        "--api-key",
      ],
      [
        options.configAction === "provider-set" ? options.endpoint : undefined,
        "--endpoint",
      ],
      [
        options.configAction === "provider-set" ? options.device : undefined,
        "--device",
      ],
      [
        options.configAction === "provider-set"
          ? options.defaultModel
          : undefined,
        "--default",
      ],
      [options.mode, "--mode"],
      [options.listen, "--listen"],
      [options.serverTokenFile, "--token-file"],
    ]);
    if (unsupported) {
      throw new Error(
        `zg config ${options.configAction === "model-set" ? "model" : "provider"} set does not accept ${unsupported}`,
      );
    }
  } else if (options.defaultModel) {
    throw new Error("--default can only be used with zg config model set");
  }

  if (command === "server") {
    if (!options.serverAction && !options.serverStdio) {
      throw new Error("zg server requires on, off, status, run, or --stdio");
    }
    if (options.serverAction && options.serverStdio) {
      throw new Error("--stdio cannot be combined with a server action");
    }
    if (
      options.listen !== undefined &&
      options.serverAction !== "run" &&
      options.serverAction !== "on" &&
      !options.serverStdio
    ) {
      throw new Error("--listen can only be used with zg server on or run");
    }
    if (
      options.serverTokenFile !== undefined &&
      options.serverAction === "status"
    ) {
      throw new Error("--token-file cannot be used with zg server status");
    }
    if (
      options.mcpToolset !== undefined &&
      options.serverAction !== "on" &&
      options.serverAction !== "run" &&
      !options.serverStdio
    ) {
      throw new Error(
        "--mcp-toolset can only be used with zg server on or run",
      );
    }
  } else if (options.mcpToolset !== undefined && command !== "install") {
    throw new Error(
      "--mcp-toolset can only be used with zg server on, run, --stdio, or zg install",
    );
  } else if (options.serverStdio !== undefined) {
    throw new Error("--stdio can only be used with zg server");
  }

  if (
    options.rg &&
    (hasExplicitRoutes(options) || options.hybridQueries?.length)
  ) {
    throw new Error(
      "--rg cannot be combined with --hybrid, --fts, or --vector",
    );
  }
  if (options.rg && options.fuse) {
    throw new Error("--rg cannot be combined with --fuse");
  }
  if (options.forceDirect && options.mode !== "direct") {
    throw new Error("--force-direct requires --mode direct");
  }

  if (options.rg && options.preview) {
    throw new Error(
      "--preview is not supported with --rg; use -A/-B/-C for rg context",
    );
  }
  if (options.rg && options.trace) {
    throw new Error("--rg cannot be combined with --trace");
  }
  if (options.rg && (options.preferSymbol || options.symbolTypes?.length)) {
    throw new Error("--rg cannot be combined with indexed symbol options");
  }
  if (
    options.rg &&
    (options.refresh || options.embeddingConcurrency !== undefined)
  ) {
    throw new Error("--rg cannot be combined with indexed refresh options");
  }
  if (!options.rg && (options.rgCompatibilityOptions?.length ?? 0) > 0) {
    const [option] = options.rgCompatibilityOptions!;
    throw new Error(`${option} can only be used with --rg`);
  }

  if (
    options.mode !== undefined &&
    command !== "query" &&
    command !== "index" &&
    command !== "status"
  ) {
    throw new Error("--mode can only be used with query, index, or status");
  }
  if (
    options.checkReady &&
    command !== "status" &&
    !(command === "server" && options.serverAction === "status")
  ) {
    throw new Error(
      "--check-ready can only be used with zg status or zg server status",
    );
  }
  if (options.listen !== undefined && command !== "server") {
    throw new Error("--listen can only be used with zg server on or run");
  }
  if (options.serverTokenFile !== undefined && command !== "server") {
    throw new Error("--token-file can only be used with zg server");
  }

  if (
    options.installTargets?.length &&
    command !== "install" &&
    command !== "uninstall"
  ) {
    throw new Error(
      "--target can only be used with zg install or zg uninstall",
    );
  }
  if (
    options.installMcpToolTimeoutSeconds !== undefined &&
    command !== "install"
  ) {
    throw new Error("--mcp-tool-timeout can only be used with zg install");
  }
  if (options.installMcpTokenEnv !== undefined && command !== "install") {
    throw new Error("--mcp-token-env can only be used with zg install");
  }
  if (options.installMcpTransport !== undefined && command !== "install") {
    throw new Error("--mcp-transport can only be used with zg install");
  }
  if (
    command === "install" &&
    options.installMcpTransport !== "http" &&
    options.installMcpTokenEnv !== undefined
  ) {
    throw new Error("--mcp-token-env requires --mcp-transport http");
  }
  if (options.force && command !== "install") {
    throw new Error("--force can only be used with zg install");
  }

  if (command === "install" || command === "uninstall") {
    const unsupported = firstEnabledOption([
      [options.color, "--color"],
      [options.home, "--home"],
      [options.embedding, "--embedding"],
      [options.modelCacheDir, "--model-cache"],
      [options.device, "--device"],
      [options.apiKey, "--api-key"],
      [options.endpoint, "--endpoint"],
      [options.rebuild, "--rebuild"],
      [options.drop, "--drop"],
      [options.resetPaths, "--reset-paths"],
      [options.globs?.length, "--glob"],
      [options.insensitiveGlobs?.length, "--iglob"],
      [options.fileTypes?.length, "--type"],
      [options.excludedFileTypes?.length, "--type-not"],
      [options.hidden, "--hidden"],
      [options.noIgnore, "--no-ignore"],
      [options.ignoreFiles?.length, "--ignore-file"],
      [options.maxDepth, "--max-depth"],
      [options.maxFileSizeBytes, "--max-filesize"],
      [options.follow, "--follow"],
      [options.embeddingConcurrency, "--embedding-concurrency"],
    ]);
    if (unsupported) {
      throw new Error(`${unsupported} is not supported with zg ${command}`);
    }
  }
  if (
    options.yes &&
    command !== "install" &&
    command !== "uninstall" &&
    !(command === "index" && options.drop)
  ) {
    throw new Error(
      "--yes is only valid for install, uninstall, or index --drop",
    );
  }

  if (options.drop && command !== "index") {
    throw new Error("--drop can only be used with zg index");
  }
  if (options.rebuild && command !== "index") {
    throw new Error("--rebuild can only be used with zg index");
  }
  if (options.resetPaths && command !== "index") {
    throw new Error("--reset-paths can only be used with zg index");
  }
  if (
    options.drop &&
    (options.rebuild ||
      options.resetPaths ||
      options.home ||
      options.embedding ||
      options.modelCacheDir ||
      options.device !== undefined ||
      options.apiKey ||
      options.endpoint ||
      options.globs?.length ||
      options.insensitiveGlobs?.length ||
      options.fileTypes?.length ||
      options.excludedFileTypes?.length ||
      options.hidden ||
      options.noIgnore ||
      options.ignoreFiles?.length ||
      options.maxDepth !== undefined ||
      options.maxFileSizeBytes !== undefined ||
      options.debug ||
      options.follow ||
      options.embeddingConcurrency)
  ) {
    throw new Error("zg index --drop cannot be combined with indexing options");
  }

  if (command === "query") {
    const unsupported = firstEnabledOption([
      [options.embedding, "--embedding"],
      [options.endpoint, "--endpoint"],
      [options.embeddingConcurrency, "--embedding-concurrency"],
    ]);
    if (unsupported) {
      throw new Error(`${unsupported} is not supported with zg query`);
    }
  }

  if (command === "query" && !options.rg) {
    const queryCount =
      positionals.length +
      (options.hybridQueries?.length ?? 0) +
      (options.routes?.length ?? 0);
    if (queryCount === 0) {
      throw new Error(
        "zg query requires a query or --hybrid/--fts/--vector route",
      );
    }
  }
}

function firstEnabledOption(
  candidates: readonly (readonly [unknown, string])[],
): string | undefined {
  return candidates.find(([value]) =>
    Array.isArray(value) ? value.length > 0 : value !== undefined,
  )?.[1];
}

function parseClientMode(value: string): "direct" | "server" | "auto" {
  if (value === "direct" || value === "server" || value === "auto")
    return value;
  throw new Error("--mode must be direct, server, or auto");
}

function parseQueryRefreshMode(value: string): QueryRefreshMode {
  if (value === "background" || value === "wait" || value === "off") {
    return value;
  }
  throw new Error("--refresh must be background, wait, or off");
}

function allowRemoteValueError(): Error {
  return new Error(
    [
      "--allow-remote does not accept a value.",
      "It authorizes Remote Embedding for the current command only.",
      "For persistent authorization, use:",
      "  zg auth grant --capability embedding --scope workspace",
    ].join("\n"),
  );
}

function parseAuthorizationCapability(value: string): "embedding" {
  if (value === "embedding") return value;
  throw new Error("--capability currently supports only embedding");
}

function parseAuthorizationScope(value: string): "workspace" {
  if (value === "workspace") return value;
  throw new Error("zg auth supports only workspace scope");
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
  return /^-[A-Za-z].*$/.test(arg) && arg !== "-h";
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
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [flag]);
        markRgCompatibilityOption(options, flag);
        break;
      case "i":
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [flag]);
        markRgCompatibilityOption(options, flag);
        break;
      case "w":
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [flag]);
        markRgCompatibilityOption(options, flag);
        break;
      case "P":
      case "S":
      case "s":
      case "a":
      case "u":
      case "U":
      case "v":
      case "x":
      case "z":
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [flag]);
        markRgCompatibilityOption(options, flag);
        break;
      case "L":
        options.follow = true;
        break;
      case "e": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        options.rgOptions = appendRgPattern(options.rgOptions, value.value);
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "g": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        options.globs = appendValue(options.globs, value.value, flag);
        return value.nextIndex;
      }
      case "E": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [
          flag,
          value.value,
        ]);
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "t":
      case "T": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        if (option === "t") {
          options.fileTypes = appendValue(options.fileTypes, value.value, flag);
        } else {
          options.excludedFileTypes = appendValue(
            options.excludedFileTypes,
            value.value,
            flag,
          );
        }
        return value.nextIndex;
      }
      case "f": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        options.rgOptions = appendRgPatternFile(
          options.rgOptions,
          value.value,
          flag,
        );
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "m": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        parseNonNegativeInteger(value.value, flag);
        options.rgOptions = appendRgExtraArgs(options.rgOptions, [
          flag,
          value.value,
        ]);
        markRgCompatibilityOption(options, flag);
        return value.nextIndex;
      }
      case "j": {
        const value = readShortOptionValue(args, index, offset, arg, flag);
        parseNonNegativeInteger(value.value, flag);
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

function appendRgPatternFile(
  existing: CliRgOptions | undefined,
  value: string,
  option: string,
): CliRgOptions {
  return {
    ...(existing ?? {}),
    patternFiles: appendValue(existing?.patternFiles, value, option),
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

  if (targets.length === 0) {
    throw new Error("--target requires at least one agent name");
  }

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

function parsePositiveInteger(value: string, option: string): number {
  if (!/^\d+$/.test(value)) {
    throw new Error(`${option} requires a positive integer`);
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) {
    throw new Error(`${option} requires a positive integer`);
  }

  return parsed;
}

function parseEnvironmentVariable(value: string, option: string): string {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(value)) {
    throw new Error(`${option} requires a valid environment variable name`);
  }
  return value;
}

function parseMcpInstallTransport(value: string): McpInstallTransport {
  if (value === "stdio" || value === "http") return value;
  throw new Error("--mcp-transport must be stdio or http");
}

function parseByteSize(value: string, option: string): number {
  const match = value.trim().match(/^(\d+)([KMGT])?$/i);
  if (!match) {
    throw new Error(`${option} requires bytes or an integer K/M/G/T size`);
  }
  const amount = Number.parseInt(match[1]!, 10);
  const suffix = match[2]?.toUpperCase();
  const multiplier =
    suffix === "K"
      ? 1024
      : suffix === "M"
        ? 1024 ** 2
        : suffix === "G"
          ? 1024 ** 3
          : suffix === "T"
            ? 1024 ** 4
            : 1;
  const bytes = amount * multiplier;
  if (!Number.isSafeInteger(bytes)) {
    throw new Error(`${option} is too large`);
  }
  return bytes;
}

function parseNonNegativeInteger(value: string, option: string): number {
  if (!/^\d+$/.test(value)) {
    throw new Error(`${option} requires a non-negative integer`);
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed)) {
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

export function parseDevice(
  value: string,
): "auto" | "cpu" | "metal" | "vulkan" | "cuda" {
  const normalized = value.trim().toLowerCase();
  if (
    normalized === "auto" ||
    normalized === "cpu" ||
    normalized === "metal" ||
    normalized === "vulkan" ||
    normalized === "cuda"
  ) {
    return normalized;
  }

  throw new Error(`Unsupported device: ${value}`);
}

function hasExplicitRoutes(options: CliOptions): boolean {
  return (options.routes?.length ?? 0) > 0;
}

function appendRoute(
  existing: ZvecGrepContextRoute[] | undefined,
  mode: ZvecGrepContextRoute["mode"],
  query: string,
  option: string,
): ZvecGrepContextRoute[] {
  const normalized = query.trim();
  if (!normalized) {
    throw new Error(`${option} requires a non-empty query`);
  }
  return [...(existing ?? []), { mode, query: normalized }];
}

function appendQuery(
  existing: string[] | undefined,
  query: string,
  option: string,
): string[] {
  const normalized = query.trim();
  if (!normalized) {
    throw new Error(`${option} requires a non-empty query`);
  }
  return [...(existing ?? []), normalized];
}

function parseSymbolType(value: string): CodeSymbolType {
  if (VALID_SYMBOL_TYPES.has(value as CodeSymbolType)) {
    return value as CodeSymbolType;
  }

  throw new Error(`Unsupported symbol type: ${value}`);
}

export function splitPathFilters(value: string): string[] {
  const filters: string[] = [];
  let start = 0;
  let braceDepth = 0;
  let bracketDepth = 0;
  let escaped = false;

  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (character === "\\") {
      escaped = true;
      continue;
    }
    if (character === "[") {
      bracketDepth += 1;
      continue;
    }
    if (character === "]" && bracketDepth > 0) {
      bracketDepth -= 1;
      continue;
    }
    if (bracketDepth === 0 && character === "{") {
      braceDepth += 1;
      continue;
    }
    if (bracketDepth === 0 && character === "}" && braceDepth > 0) {
      braceDepth -= 1;
      continue;
    }
    if (character !== "," || braceDepth > 0 || bracketDepth > 0) {
      continue;
    }

    const filter = value.slice(start, index).trim();
    if (filter.length > 0) {
      filters.push(filter);
    }
    start = index + 1;
  }

  const finalFilter = value.slice(start).trim();
  if (finalFilter.length > 0) {
    filters.push(finalFilter);
  }
  return filters;
}

function appendValue(
  existing: string[] | undefined,
  value: string,
  option: string,
): string[] {
  const normalized = value.trim();
  if (!normalized) {
    throw new Error(`${option} requires a non-empty value`);
  }
  return [...(existing ?? []), normalized];
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
