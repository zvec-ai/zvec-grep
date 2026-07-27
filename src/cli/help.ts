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

function mainHelp(version: string): string {
  return `zvec-grep ${version}

Usage:
  zg <command> [options]

Commands:
  query          Search indexed context or run managed ripgrep
  index          Build, rebuild, or drop the workspace index
  status         Show workspace and index status
  collections    Manage named collections
  config         Configure per-model local embedding runtime settings
  auth           Manage Workspace Remote Embedding authorization
  server         Start, stop, inspect, or run the shared MCP server
  install        Install agent integrations
  uninstall      Remove agent integrations
  help           Show help for a command
  version        Print the installed version

Examples:
  zg query "where authentication is validated"
  zg query --fts "AuthService"
  zg query --rg -F "AuthService" src
  zg index --embedding local/embeddinggemma-300m
  zg status
  zg auth status
  zg server on
  zg config model set local/embeddinggemma-300m --llama-gpu metal
  zg install

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

Result options:
  --limit <n>                       Maximum results
  --human                           Human-readable ranked output
  --preview <none|short|full>       Indexed source preview size
  --debug                           Print diagnostics to stderr
  --trace                           Include per-hit indexed search trace
  --refresh <background|wait|off>   Refresh policy (Server: background; Direct: off)
  --mode <direct|server|auto>       Select indexed query transport
  --allow-remote                    Allow Remote Embedding for this command only

Direct mode warns and uses off when background is requested.

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
Options that replace rg's output format are rejected.`;
    case "index":
      return `Usage:
  zg index [root] [options]
  zg index [root] --rebuild [options]
  zg index [root] --drop [--yes]

Options:
  --embedding <model>               Model such as local/embeddinggemma-300m or qwen/text-embedding-v4
  --rebuild                         Rebuild the existing index
  --drop                            Permanently remove the workspace index
  --yes                             Confirm --drop without prompting
  --mode <direct|server|auto>       Select indexing transport
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
  --embedding-concurrency <n>       Embedding task concurrency
  --api-key <key>                   Embedding provider API key
  --endpoint <url>                  Embedding provider endpoint
  --model-cache <path>              Local model cache directory
  --gpu                             Try GPU acceleration
  --no-gpu                          Force CPU local embeddings
  --llama-gpu <mode>                auto, metal, vulkan, cuda, off
  --embedding-parallelism <n>       Local embedding context parallelism
  --allow-remote                    Allow Remote Embedding for this command only

New indexes require --embedding, ZVEC_GREP_EMBEDDING, or a configured default.
Existing indexes reuse their stored embedding schema.`;
    case "status":
      return `Usage:
  zg status [root] [--mode <direct|server|auto>] [--check-ready]

Shows the nearest workspace root, index policy, index state, embedding schema,
stored paths, refresh status, and suggested next action.

--check-ready preserves the normal output and exits non-zero unless the
Workspace index is ready.`;
    case "collections":
      return `Usage:
  zg collections
  zg collections info <name>
  zg collections index <name> [root] [options]
  zg collections remove <name>

Named collections support the same embedding, file-selection, discovery,
rebuild, and embedding-concurrency options as zg index.`;
    case "config":
      return `Usage:
  zg config model set <local/model> [--gpu|--no-gpu|--llama-gpu <mode>] [--embedding-parallelism <n>]

Stores runtime GPU and parallelism settings for one local embedding model in
~/.zvec-grep/config.json. These settings do not change index compatibility.`;
    case "auth":
      return `Usage:
  zg auth grant [root] --capability embedding --scope workspace [--embedding <model>]
  zg auth status [root]
  zg auth revoke [root]

Manage the signed Remote Embedding grant stored in the Workspace under
.zvec-grep/authorization.json. Workspace grants are shared by zg CLI and zg MCP.

Scopes used during operations:
  once                              Current CLI command or Agent tool call only
  workspace                         Persisted in this Workspace

Use --allow-remote on zg query or zg index to authorize Remote Embedding for
that command only. This authorization is not persisted. API credentials
configure a provider but do not grant permission.`;
    case "server":
      return `Usage:
  zg server on [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset <agent|full>]
  zg server off [--token-file <path>]
  zg server status [--check-ready]
  zg server run [--listen 127.0.0.1:7999] [--token-file <path>] [--mcp-toolset <agent|full>]

The server listens on loopback. Authentication is disabled by default; pass a
token file or set ZVEC_GREP_SERVER_TOKEN to require Bearer authentication.
The public MCP endpoint defaults to the agent toolset (search and rg only).
Use --mcp-toolset full, or ZVEC_GREP_MCP_TOOLSET=full, for compatibility with
clients that require the four index and status tools. CLI index and status
commands continue to use the daemon's internal administration endpoint.
--check-ready exits non-zero unless the server is ready.`;
    case "install":
      return `Usage:
  zg install [--target codex|claude|opencode|cursor|all|auto] [--yes] [--force]

Options:
  --target <agent>                  codex, claude, opencode, cursor, auto, or all; repeatable
  --mcp-tool-timeout <seconds>      Codex MCP tool timeout (default: 600)
  --mcp-token-env <name>            Bearer token environment variable
  --yes                             Install detected agents without prompting
  --force                           Replace conflicting unmanaged configuration

Interactive setup detects supported agents, configures the local MCP server,
and starts it. Codex and Claude Code also receive managed guidance and local
tool pre-approval. Remote Embedding authorization remains separate and is
requested by zvec-grep on first remote use. This does not install the npm package.`;
    case "uninstall":
      return `Usage:
  zg uninstall [--target codex|claude|opencode|cursor|all|auto] [--yes]

Removes zvec-grep-managed MCP configuration, trust, and guidance.`;
    case "help":
      return `Usage:
  zg help [command]
  zg <command> --help
  zg -h
  zg --help`;
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
