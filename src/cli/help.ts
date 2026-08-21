import {
  COMPONENT_CODE_FORMATS,
  STRUCTURED_CODE_FORMATS,
} from "../engine/code-formats.js";
import {
  listKnownBinaryExtensionGroups,
  listRecognizedFileTypes,
  type RecognizedFileType,
} from "../engine/file-type.js";
import { resolveMaxFileSizeBytes } from "../engine/file-size-policy.js";
import { listEmbeddingModels } from "../engine/models/index.js";

type EmbeddingCatalogEntry = ReturnType<typeof listEmbeddingModels>[number];

export function printHelp(version: string, topic?: string): void {
  if (!topic) {
    console.log(mainHelp(version));
    return;
  }

  const help = commandHelp(topic);
  if (!help) {
    throw new Error(`Unknown help topic: ${topic}`);
  }
  console.log(help);
}

const ENVIRONMENT_VARIABLES = {
  ZVEC_GREP_HOME:
    "Runtime and daemon state directory; Workspace indexes stay under <root>/.zvec-grep",
  ZVEC_GREP_MODE: "Default client mode: direct, server, or auto",
  ZVEC_GREP_SERVER_URL: "MCP server URL used by CLI clients",
  ZVEC_GREP_SERVER_TOKEN: "Server/client Bearer token",
  ZVEC_GREP_SERVER_TOKEN_FILE: "File containing the Server/client Bearer token",
  ZVEC_GREP_MCP_TOOLSET: "Server MCP surface: agent or full",
  ZVEC_GREP_EMBEDDING: "Default model for new indexes and auth grant",
  ZVEC_GREP_API_KEY: "Embedding provider credential fallback",
  ZVEC_GREP_ENDPOINT: "Remote Embedding endpoint fallback",
  ZVEC_GREP_MODEL_CACHE: "Local embedding model cache directory",
  ZVEC_GREP_DEVICE: "Local embedding device: auto, cpu, metal, vulkan, or cuda",
  DASHSCOPE_API_KEY: "Qwen credential fallback after ZVEC_GREP_API_KEY",
  QWEN_API_KEY: "Qwen credential fallback after DASHSCOPE_API_KEY",
  ZVEC_GREP_AUTHORIZATION_KEY_FILE:
    "Workspace grant signing-key file (advanced)",
  ZVEC_GREP_METAL_KEEP_RESIDENCY:
    "Set to 1 to keep llama.cpp Metal residency enabled (advanced)",
  ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM:
    "Positive llama.cpp context parallelism override (advanced)",
  NO_COLOR: "Disable terminal colors",
  CODEX_HOME: "Codex configuration directory used by zg install",
  CLAUDE_CONFIG_DIR: "Claude configuration directory used by zg install",
  OPENCODE_CONFIG: "OpenCode configuration file used by zg install",
  CURSOR_CONFIG_DIR: "Cursor configuration directory used by zg install",
} as const;

type EnvironmentVariableName = keyof typeof ENVIRONMENT_VARIABLES;

const MAIN_ENVIRONMENT_VARIABLES: readonly EnvironmentVariableName[] = [
  "ZVEC_GREP_HOME",
  "ZVEC_GREP_MODE",
  "ZVEC_GREP_EMBEDDING",
  "ZVEC_GREP_API_KEY",
  "ZVEC_GREP_SERVER_URL",
];

function mainHelp(version: string): string {
  return `zvec-grep ${version}

Usage:
  zg <command> [options]

Commands:
  query          Search indexed context or run managed ripgrep
  explore        Build a code-graph context pack for a symbol/query
  callers        List callers of a symbol (code graph)
  callees        List callees of a symbol (code graph)
  impact         List reverse dependents (calls/refs) of a symbol
  index          Build, rebuild, or drop the workspace index
  status         Show workspace and index status
  config         Configure provider credentials and embedding model defaults
  auth           Manage Workspace Remote Embedding authorization
  server         Start, stop, inspect, or run the shared MCP server
  install        Install agent integrations
  uninstall      Remove agent integrations
  help           Show help for a command or topic
  version        Print the installed version

Examples:
  zg query "where authentication is validated"
  zg explore AuthService
  zg callers validateToken --depth 2
  zg impact formatDate
  zg query --fts "AuthService"
  zg query --rg -F "AuthService" src
  zg index --embedding local/potion-code-16m-v2
  zg status
  zg auth status
  zg server on
  zg config model set local/potion-code-16m-v2 --device metal
  zg install

Environment:
${formatEnvironmentVariables(MAIN_ENVIRONMENT_VARIABLES)}

Run zg help models or zg help file-types for supported indexing capabilities.
Run zg help environment for all variables, scopes, aliases, and precedence.
Run zg help <command> or zg <command> --help for command-specific help.
Use zg -h/--help for this page and zg -v/--version for the version.`;
}

function commandHelp(topic: string): string | undefined {
  switch (topic) {
    case "query":
      return `Usage:
  zg query <query> [options]
  zg query --hybrid <query> --fts <query> --vector <query> [--fuse]
  zg query --rg [rg-options] <pattern> [path...]

Search routes:
  positional query                  Hybrid FTS and vector search
  --hybrid <query>                  Add an explicit hybrid query
  --fts <query>                     Add an exact/lexical query
  --vector <query>                  Add a semantic/vector query
  --fuse                            Fuse all query groups into one ranked list
  --rg                              Run exhaustive managed ripgrep

Indexed query also expands call-graph neighbors of top hits when a code graph
is available (built during zg index). Neighbors appear as matchedBy=graph.

Result options:
  --limit <n>                       Maximum results per group (default: 7)
  --human                           Human-readable output (default: agent markdown)
  --preview <none|short|full>       Indexed preview size (default: none; --human: full)
  --debug                           Print diagnostics to stderr
  --trace                           Include per-hit indexed search trace
  --refresh <background|wait|off>   Refresh policy (defaults: server=background, direct=off)
                                    In direct mode, background warns and falls back to off
  --mode <direct|server|auto>       Select indexed query transport (default: auto)

Indexed results are shown by query group, preserving each group's own rank.
A result that matches more than one group is shown in each matching group.

Embedding runtime:
  --api-key <key>                   Embedding provider API key
  --model-cache <path>              Local model cache directory
  --device <device>                 auto, cpu, metal, vulkan, cuda
  --allow-remote                    Allow Remote Embedding for this command only

File filters:
  -g, --glob <glob>                 Include paths; prefix with ! to exclude; repeatable
  --iglob <glob>                    Case-insensitive path glob; repeatable
  -t, --type <type>                 Include a ripgrep file type; repeatable
  -T, --type-not <type>             Exclude a ripgrep file type; repeatable
  --modified-after <time>           Only files modified after a date or epoch milliseconds
  --modified-before <time>          Only files modified before a date or epoch milliseconds
  --symbol-type <type>              module, class, interface, function, value, alias
  --prefer-symbol                   Prefer exact indexed symbols

Managed --rg supports common ripgrep matching, context, engine, encoding,
discovery, glob, and type flags. Use -e when a pattern begins with "-".
Options that replace rg's output format are rejected.

Environment:
${formatEnvironmentVariables([
  "ZVEC_GREP_MODE",
  "ZVEC_GREP_API_KEY",
  "ZVEC_GREP_ENDPOINT",
  "ZVEC_GREP_MODEL_CACHE",
  "ZVEC_GREP_DEVICE",
])}

See zg help environment for precedence and Server-mode scope.`;
    case "explore":
      return `Usage:
  zg explore <symbol-or-query> [options]

Builds a CodeGraph-style context pack from the workspace code graph:
seed symbols → type hierarchy → deep neighborhood → RWR file ranking →
zvec entity-content assembly (grouped by file).

Uses the nearest workspace index from the current directory.

Options:
  --limit <n>                       Max seed symbols (default 8)
  --depth <n>                       Traversal depth (default 3)
  --max-files <n>                   Max files in the pack (default 8)
  --seed-id <id>                    Disambiguate when multiple symbols match`;
    case "callers":
    case "callees":
    case "impact":
      return `Usage:
  zg ${topic} <symbol> [options]

Query the workspace code graph for ${topic} of a symbol.
Requires a built index (zg index). Neighbors are resolved by exact symbol name.
Uses the nearest workspace index from the current directory.

Options:
  --depth <n>                       Traversal depth (default 1)
  --limit <n>                       Max neighbors (default 20)
  --seed-id <id>                    Disambiguate when multiple symbols match`;
    case "index":
      return `Usage:
  zg index [root] [options]
  zg index [root] --rebuild [options]
  zg index [root] --drop [--yes]

Index options:
  --rebuild                         Rebuild the existing index
  --drop                            Permanently remove the workspace index
  --yes                             Confirm --drop without prompting
  --debug                           Print skipped-file diagnostics to stderr
  --mode <direct|server|auto>       Select indexing transport

Embedding options:
  --embedding <model>               Model such as local/potion-code-16m-v2 or qwen/text-embedding-v4
  --api-key <key>                   Embedding provider API key
  --endpoint <url>                  Embedding provider endpoint
  --model-cache <path>              Local model cache directory
  --device <device>                 auto, cpu, metal, vulkan, cuda
  --embedding-concurrency <n>       Embedding task concurrency
  --allow-remote                    Allow Remote Embedding for this command only

File selection:
  -g, --glob <glob>                 Include paths; prefix with ! to exclude; repeatable
  --iglob <glob>                    Case-insensitive path glob; repeatable
  -t, --type <type>                 Include a ripgrep file type; repeatable
  -T, --type-not <type>             Exclude a ripgrep file type; repeatable
  --hidden                          Include hidden paths except .git and .zvec-grep
  --no-ignore                       Do not apply default or .gitignore rules
  --ignore-file <path>              Add an explicit ignore file; repeatable
  --max-depth <n>                   Maximum directory depth
  --max-filesize <size>             Maximum bytes or K/M/G/T size
  -L, --follow                      Follow symbolic links safely
  --reset-paths                     Clear inherited file-selection settings

New indexes require --embedding, ZVEC_GREP_EMBEDDING, or a configured default.
Existing indexes reuse their stored embedding schema.

Environment:
${formatEnvironmentVariables([
  "ZVEC_GREP_MODE",
  "ZVEC_GREP_EMBEDDING",
  "ZVEC_GREP_API_KEY",
  "ZVEC_GREP_ENDPOINT",
  "ZVEC_GREP_MODEL_CACHE",
  "ZVEC_GREP_DEVICE",
])}

See zg help environment for precedence and Server-mode scope.`;
    case "status":
      return `Usage:
  zg status [root] [--mode <direct|server|auto>] [--check-ready]

Shows the nearest workspace root, index policy, index state, embedding schema,
stored paths, refresh status, and suggested next action.

--check-ready preserves the normal output and exits non-zero unless the
Workspace index is ready.`;
    case "config":
      return `Usage:
  zg config provider set <provider> --api-key <key>
  zg config model set <model> [--endpoint <url> | --device <device>] [--default]

Provider options:
  --api-key <key>                   Default API key for the provider

Model options:
  --endpoint <url>                  Endpoint for a remote embedding model
  --device <device>                 Local device: auto, cpu, metal, vulkan, cuda
  --default                         Use this model for new indexes

Remote models support --endpoint; local models support --device. At least one
model option is required. --default may be used alone or with a runtime option.
Existing indexes continue to use their stored model.

Global configuration is stored in ~/.zvec-grep/config.json.`;
    case "auth":
      return `Usage:
  zg auth grant [root] --capability embedding --scope workspace [--embedding <model>]
  zg auth status [root]
  zg auth revoke [root]

Manage the signed Remote Embedding grant stored in the Workspace under
.zvec-grep/authorization.json. Workspace grants are shared by zg CLI and zg MCP.

--embedding selects the Remote Embedding model to authorize; it does not run
embedding. If omitted, auth grant uses the existing Workspace index model, then
ZVEC_GREP_EMBEDDING, then the global default.

Scopes used during operations:
  once                              Current CLI command or Agent tool call only
  workspace                         Persisted in this Workspace

Use --allow-remote on zg query or zg index to authorize Remote Embedding for
that command only. This authorization is not persisted. API credentials
configure a provider but do not grant permission.

Environment used by auth grant:
${formatEnvironmentVariables([
  "ZVEC_GREP_EMBEDDING",
  "ZVEC_GREP_API_KEY",
  "ZVEC_GREP_ENDPOINT",
  "ZVEC_GREP_AUTHORIZATION_KEY_FILE",
])}`;
    case "server":
      return `Usage:
  zg server --stdio [--token-file <path>] [--mcp-toolset <agent|full>]
  zg server on [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset <agent|full>]
  zg server off [--token-file <path>]
  zg server status [--check-ready]
  zg server run [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset <agent|full>]

--stdio is the MCP client bootstrap transport. It safely starts or reuses the
shared daemon, proxies MCP over stdin/stdout, and leaves the daemon running
when the client disconnects.

The server listens on loopback. Authentication is disabled by default; pass a
token file or set ZVEC_GREP_SERVER_TOKEN to require Bearer authentication.
The public MCP endpoint defaults to the agent toolset (indexed search only).
Use --mcp-toolset full, or ZVEC_GREP_MCP_TOOLSET=full, to expose managed rg and
the four index and status tools. CLI managed rg, index, and status commands
continue to use the daemon's internal administration endpoint.
--check-ready exits non-zero unless the server is ready.

Environment:
${formatEnvironmentVariables([
  "ZVEC_GREP_HOME",
  "ZVEC_GREP_SERVER_URL",
  "ZVEC_GREP_SERVER_TOKEN",
  "ZVEC_GREP_SERVER_TOKEN_FILE",
  "ZVEC_GREP_MCP_TOOLSET",
])}

See zg help environment for daemon startup scope.`;
    case "install":
      return `Usage:
  zg install [--target codex|claude|opencode|cursor|all|auto] [--mcp-transport stdio|http] [--mcp-toolset agent|full] [--yes] [--force]

Options:
  --target <agent>                  codex, claude, opencode, cursor, auto, or all; repeatable
  --mcp-transport <stdio|http>      MCP connection mode (default: stdio)
  --mcp-toolset <agent|full>        Daemon MCP toolset (default: agent)
  --mcp-tool-timeout <seconds>      Codex MCP tool timeout (default: 600)
  --mcp-token-env <name>            HTTP mode Bearer token environment variable
  --yes                             Install detected agents without prompting
  --force                           Replace conflicting unmanaged configuration

Interactive setup detects supported agents, configures stdio by default, and
starts the shared daemon. In stdio mode an agent reconnect also starts the
daemon automatically after a reboot. HTTP users manage later daemon restarts.
Codex, Claude Code, and OpenCode also receive managed guidance;
Codex and Claude Code receive local tool pre-approval. Remote Embedding
authorization remains separate and is requested by zvec-grep on first remote
use. This does not install the npm package.`;
    case "uninstall":
      return `Usage:
  zg uninstall [--target codex|claude|opencode|cursor|all|auto] [--yes]

Removes zvec-grep-managed MCP configuration, trust, and guidance.`;
    case "help":
      return `Usage:
  zg help [command|topic]
  zg <command> --help
  zg -h
  zg --help

Topics:
  models                             Supported embedding models
  file-types                         Supported file types and structural parsing
  environment, env                   Environment variables and precedence`;
    case "models":
      return modelsHelp();
    case "file-types":
      return fileTypesHelp();
    case "environment":
    case "env":
      return environmentHelp();
    case "version":
      return `Usage:
  zg version
  zg version -v
  zg -v
  zg --version`;
    default:
      return undefined;
  }
}

function modelsHelp(): string {
  const models = listEmbeddingModels().sort((left, right) => {
    const leftRuntime = left.provider === "local" ? 0 : 1;
    const rightRuntime = right.provider === "local" ? 0 : 1;
    return (
      leftRuntime - rightRuntime ||
      left.reference.localeCompare(right.reference)
    );
  });

  return `Usage:
  zg help models

Supported embedding models:
${formatEmbeddingModels(models)}

Local models are downloaded to the model cache on first use. Remote models
require provider credentials plus --allow-remote or a Workspace authorization.
Only qwen/qwen3-vl-embedding accepts image input.

Existing indexes keep their stored model. See zg help environment for
new-index model selection and runtime precedence.`;
}

function fileTypesHelp(): string {
  const types = listRecognizedFileTypes();
  const structuredFormats: ReadonlySet<string> = new Set(
    STRUCTURED_CODE_FORMATS,
  );
  const componentFormats: ReadonlySet<string> = new Set(COMPONENT_CODE_FORMATS);
  const codeTypes = types.filter((type) => type.kind === "code");
  const structuredCode = codeTypes.filter((type) =>
    structuredFormats.has(type.format),
  );
  const componentCode = codeTypes.filter((type) =>
    componentFormats.has(type.format),
  );
  const plainCode = codeTypes.filter(
    (type) =>
      !structuredFormats.has(type.format) && !componentFormats.has(type.format),
  );
  const documentsAndData = types.filter(
    (type) => type.kind === "text" || type.kind === "data",
  );
  const images = types.filter((type) => type.kind === "image");
  const skipped = listKnownBinaryExtensionGroups().map((group) => [
    group.label,
    group.extensions.join(", "),
  ]);
  const sizeLimits = (
    [
      ["Code", "code"],
      ["Text", "text"],
      ["Data", "data"],
      ["Image", "image"],
    ] as const
  ).map(([label, kind]) => [
    label,
    `${resolveMaxFileSizeBytes(kind) / (1024 * 1024)} MiB`,
  ]);

  return `Usage:
  zg help file-types

Structured code (symbols and scopes):
${formatFileTypeTable(structuredCode)}

Component code (JavaScript and TypeScript script blocks):
${formatFileTypeTable(componentCode)}

Other code (plain-text chunks):
${formatFileTypeTable(plainCode)}

Documents and data:
${formatFileTypeTable(documentsAndData)}
  Markdown preserves heading structure; other formats use text chunks.

Images (multimodal embedding required):
${formatFileTypeTable(images)}
  Images are ignored by default and must be explicitly selected.

Other text:
  Unknown non-binary extensions and extensionless files use text chunks.

Skipped binary types:
${formatTable(["GROUP", "EXTENSIONS"], skipped)}
  Files detected as binary by content are also skipped.

Indexing rules:
  Default size limits:
${formatTable(["KIND", "MAX SIZE"], sizeLimits)}
  Empty files are skipped. Use --max-filesize to override the size limit.
  Common dependencies, build output, generated files, and lock files are
  ignored by default. .git and .zvec-grep are always skipped.`;
}

function formatEmbeddingModels(
  models: readonly EmbeddingCatalogEntry[],
): string {
  const rows = models.map((model) => {
    const runtime = model.provider === "local" ? "local" : "remote";
    const input =
      "kind" in model && model.kind === "multimodal" ? "text,image" : "text";
    const inputLimit =
      "maxInputTokens" in model ? model.maxInputTokens : model.contextSize;
    return [
      model.reference,
      runtime,
      input,
      String(model.dimension),
      String(inputLimit),
      model.backend,
    ];
  });
  return formatTable(
    ["MODEL", "RUNTIME", "INPUT", "DIMS", "TOKENS", "BACKEND"],
    rows,
    new Set([3, 4]),
  );
}

function formatFileTypeTable(types: readonly RecognizedFileType[]): string {
  const rows = [...types]
    .sort((left, right) => left.format.localeCompare(right.format))
    .map((type) => [type.format, type.patterns.join(", ")]);
  return formatTable(["TYPE", "FILES"], rows);
}

function formatTable(
  header: readonly string[],
  rows: readonly (readonly string[])[],
  rightAlignedColumns: ReadonlySet<number> = new Set(),
): string {
  const widths = header.map((label, index) =>
    Math.max(label.length, ...rows.map((row) => row[index]!.length)),
  );
  const separator = widths.map((width) => "-".repeat(width));

  return [header, separator, ...rows]
    .map(
      (row) =>
        `  ${row
          .map((value, index) =>
            index === row.length - 1
              ? value
              : rightAlignedColumns.has(index)
                ? value.padStart(widths[index]!)
                : value.padEnd(widths[index]!),
          )
          .join("  ")}`,
    )
    .join("\n");
}

function environmentHelp(): string {
  return `Usage:
  zg help environment
  zg help env

Client and Server:
${formatEnvironmentVariables([
  "ZVEC_GREP_MODE",
  "ZVEC_GREP_SERVER_URL",
  "ZVEC_GREP_SERVER_TOKEN",
  "ZVEC_GREP_SERVER_TOKEN_FILE",
  "ZVEC_GREP_MCP_TOOLSET",
])}

Embedding:
${formatEnvironmentVariables([
  "ZVEC_GREP_EMBEDDING",
  "ZVEC_GREP_API_KEY",
  "ZVEC_GREP_ENDPOINT",
  "ZVEC_GREP_MODEL_CACHE",
  "ZVEC_GREP_DEVICE",
])}

Qwen credential aliases:
${formatEnvironmentVariables(["DASHSCOPE_API_KEY", "QWEN_API_KEY"])}

State and authorization:
${formatEnvironmentVariables([
  "ZVEC_GREP_HOME",
  "ZVEC_GREP_AUTHORIZATION_KEY_FILE",
])}

Advanced:
${formatEnvironmentVariables([
  "ZVEC_GREP_METAL_KEEP_RESIDENCY",
  "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM",
  "NO_COLOR",
])}

Agent integration paths:
${formatEnvironmentVariables([
  "CODEX_HOME",
  "CLAUDE_CONFIG_DIR",
  "OPENCODE_CONFIG",
  "CURSOR_CONFIG_DIR",
])}

Precedence:
  Embedding runtime                 CLI > Workspace snapshot > Global config > Environment
  New-index model                  --embedding > ZVEC_GREP_EMBEDDING > Global config
  Client mode                      --mode > ZVEC_GREP_MODE > Global config
  Qwen environment credential      ZVEC_GREP_API_KEY > DASHSCOPE_API_KEY > QWEN_API_KEY

Server scope:
  zg index forwards its ZVEC_GREP_EMBEDDING default to Server and auto modes.
  Direct MCP calls use the embedding environment inherited by the daemon.
  Restart the daemon after changing its embedding runtime environment.

Explicit CLI options take priority. Help output never prints or stores
environment values.`;
}

function formatEnvironmentVariables(
  names: readonly EnvironmentVariableName[],
): string {
  const width = Math.max(...names.map((name) => name.length)) + 2;
  return names
    .map((name) => `  ${name.padEnd(width)}${ENVIRONMENT_VARIABLES[name]}`)
    .join("\n");
}
