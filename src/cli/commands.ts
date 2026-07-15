import { randomUUID } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  readlink,
  rename,
  stat,
  unlink,
} from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import {
  createZvecGrep,
  type CreateZvecGrepOptions,
  type IndexProgress,
  type RootPath,
  type ZvecGrepContextOptions,
} from "../index.js";
import {
  globalConfigPath,
  updateGlobalConfigFromExplicitOptions,
} from "../engine/config.js";
import { findNearestAnonymousWorkspace } from "../engine/service/root.js";
import type { ParsedArgs, CliOptions } from "./types.js";
import {
  contextWarningLines,
  printAgentContextResult,
  printHumanContextResult,
} from "./format/context.js";
import { printDebug } from "./format/debug.js";
import { createIndexProgressReporter } from "./format/progress.js";
import {
  printAnonymousInfo,
  printCollectionInfo,
  printCollectionList,
  printCollectionRemoveResult,
  printIndexPathFilterTip,
  printIndexResult,
} from "./format/status.js";

type AgentInstaller = {
  id: string;
  label: string;
  description: string;
  install: (options: InstallAgentOptions) => Promise<InstallAgentResult>;
};

type InstallAgentOptions = {
  force: boolean;
  mcpToolTimeoutSeconds: number;
};

type InstallAgentResult = {
  files: string[];
};

const AGENT_INSTALLERS: readonly AgentInstaller[] = [
  {
    id: "codex",
    label: "Codex",
    description: "configure zvec-grep MCP and Codex guidance",
    install: installCodexIntegration,
  },
];

const ZVEC_GREP_CONFIG_START = "# ZVEC_GREP_START";
const ZVEC_GREP_CONFIG_END = "# ZVEC_GREP_END";
const ZVEC_GREP_AGENTS_START = "<!-- ZVEC_GREP_START -->";
const ZVEC_GREP_AGENTS_END = "<!-- ZVEC_GREP_END -->";
const DEFAULT_MCP_TOOL_TIMEOUT_SECONDS = 600;

export async function runParsedCommand(parsed: ParsedArgs): Promise<void> {
  if (parsed.options.install) {
    await runInstall(parsed);
    return;
  }

  if (parsed.options.index) {
    await runIndex(parsed);
    return;
  }

  if (parsed.options.disableIndex) {
    await runDisableIndex(parsed);
    return;
  }

  if (parsed.options.status) {
    await runStatus(parsed);
    return;
  }

  if (parsed.options.collections) {
    await runCollections(parsed);
    return;
  }

  if (parsed.options.serve) {
    await runServe(parsed);
    return;
  }

  await runQuery(parsed);
}

async function runInstall(parsed: ParsedArgs): Promise<void> {
  const installers = await resolveInstallers(parsed);
  if (installers.length === 0) {
    console.log("No agents selected.");
    return;
  }

  console.log(
    `Installing zvec-grep for: ${installers.map((installer) => installer.label).join(", ")}`,
  );
  for (const installer of installers) {
    const result = await installer.install({
      force: parsed.options.force === true,
      mcpToolTimeoutSeconds:
        parsed.options.installMcpToolTimeoutSeconds ??
        DEFAULT_MCP_TOOL_TIMEOUT_SECONDS,
    });
    console.log(`Installed ${installer.label}:`);
    for (const file of result.files) {
      console.log(`  ${file}`);
    }
  }

  console.log(
    "Restart the selected agent or start a new session to pick up the integration.",
  );
  if (installers.some((installer) => installer.id === "codex")) {
    console.log("Codex MCP server: zg serve --mcp");
  }
}

async function installCodexIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const codexHome = resolveCodexHome();
  const configPath = resolve(codexHome, "config.toml");
  const agentsPath = resolve(codexHome, "AGENTS.md");

  await writeMarkedFile({
    path: configPath,
    startMarker: ZVEC_GREP_CONFIG_START,
    endMarker: ZVEC_GREP_CONFIG_END,
    block: codexConfigBlock(options.mcpToolTimeoutSeconds),
    force: options.force,
    hasConflict: hasCodexMcpServerConfig,
    conflictMessage: `Existing [mcp_servers.zvec_grep] found in ${configPath}. Re-run with --force after removing or moving that table into the zvec-grep managed block.`,
    removeConflict: removeCodexMcpServerConfig,
  });

  await writeMarkedFile({
    path: agentsPath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
    block: codexAgentsBlock(),
    force: true,
  });

  return { files: [configPath, agentsPath] };
}

async function resolveInstallers(
  parsed: ParsedArgs,
): Promise<AgentInstaller[]> {
  const targetTokens = [
    ...(parsed.options.installTargets ?? []),
    ...parsed.positionals,
  ];

  if (targetTokens.length > 0) {
    return installersFromTokens(targetTokens);
  }

  if (
    parsed.options.yes === true ||
    !process.stdin.isTTY ||
    !process.stdout.isTTY
  ) {
    return installersFromTokens(["auto"]);
  }

  return promptInstallers();
}

async function promptInstallers(): Promise<AgentInstaller[]> {
  console.log("Select agents to configure:");
  AGENT_INSTALLERS.forEach((installer, index) => {
    console.log(
      `  ${index + 1}. ${installer.label} - ${installer.description}`,
    );
  });
  console.log("  all. All supported agents");
  console.log("  none. Cancel");

  const readline = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  try {
    const answer = await readline.question("Agents [codex]: ");
    const normalized = answer.trim();
    return installersFromTokens(
      normalized.length > 0 ? splitTargetTokens(normalized) : ["codex"],
    );
  } finally {
    readline.close();
  }
}

function installersFromTokens(tokens: readonly string[]): AgentInstaller[] {
  const normalized = tokens.flatMap(splitTargetTokens);
  if (normalized.length === 0) {
    return installersFromTokens(["auto"]);
  }

  const selected = new Map<string, AgentInstaller>();
  for (const token of normalized) {
    const lower = token.toLowerCase();
    if (lower === "none") {
      return [];
    }

    if (lower === "auto" || lower === "all") {
      for (const installer of AGENT_INSTALLERS) {
        selected.set(installer.id, installer);
      }
      continue;
    }

    const numbered = Number.parseInt(lower, 10);
    if (
      Number.isInteger(numbered) &&
      numbered >= 1 &&
      numbered <= AGENT_INSTALLERS.length
    ) {
      const installer = AGENT_INSTALLERS[numbered - 1]!;
      selected.set(installer.id, installer);
      continue;
    }

    const installer = AGENT_INSTALLERS.find(
      (candidate) =>
        candidate.id === lower || candidate.label.toLowerCase() === lower,
    );
    if (!installer) {
      throw new Error(`Unknown install target: ${token}`);
    }

    selected.set(installer.id, installer);
  }

  return [...selected.values()];
}

function splitTargetTokens(value: string): string[] {
  return value
    .split(/[,\s]+/)
    .map((target) => target.trim())
    .filter((target) => target.length > 0);
}

async function runIndex(parsed: ParsedArgs): Promise<void> {
  const explicitRoot = parsed.positionals.length > 0;
  const root = resolveIndexRoot(parsed.positionals[0]);
  const rootPath = indexRootPath(root, parsed.options);
  if (parsed.positionals.length > 1) {
    throw new Error("zg --index accepts at most one root path");
  }

  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, rootPath.absolutePath),
  );
  const progress = createIndexProgressReporter();
  try {
    printIndexPathFilterTip(parsed.options);
    const result = await zvecGrep.index({
      root: rootPath.absolutePath,
      rootPaths: explicitRoot ? [rootPath] : undefined,
      rebuild: parsed.options.rebuild,
      resetPaths: parsed.options.resetPaths,
      includePaths: parsed.options.includePaths,
      excludePaths: parsed.options.excludePaths,
      embeddingConcurrency: parsed.options.embeddingConcurrency,
      onProgress: progress.report,
    });
    progress.finish();
    const info = await zvecGrep.info({ root: rootPath.absolutePath });
    printIndexResult(
      "Indexed anonymous workspace",
      result,
      parsed.options,
      info.collection?.rootPaths,
    );
    persistExplicitGlobalConfig(
      parsed.options,
      info.collection?.embedding?.provider,
    );
  } catch (error) {
    progress.finish();
    throw error;
  } finally {
    await zvecGrep.close();
  }
}

async function runServe(parsed: ParsedArgs): Promise<void> {
  if (parsed.positionals.length > 0) {
    throw new Error("zg serve --mcp does not accept positional arguments");
  }

  const { runMcpServer } = await import("./mcp.js");
  await runMcpServer(createServiceOptions(parsed.options, process.cwd()));
}

async function runDisableIndex(parsed: ParsedArgs): Promise<void> {
  const root = resolveIndexRoot(parsed.positionals[0]);
  if (parsed.positionals.length > 1) {
    throw new Error("zg --disable-index accepts at most one root path");
  }

  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, root),
  );
  try {
    const info = await zvecGrep.disableIndex({ root });
    printAnonymousInfo(info, parsed.options);
  } finally {
    await zvecGrep.close();
  }
}

async function runStatus(parsed: ParsedArgs): Promise<void> {
  const root = parsed.positionals[0] ?? process.cwd();
  if (parsed.positionals.length > 1) {
    throw new Error("zg --status accepts at most one root path");
  }

  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, root),
  );
  try {
    const info = await zvecGrep.info({ root });
    printAnonymousInfo(info, parsed.options);
  } finally {
    await zvecGrep.close();
  }
}

async function runCollections(parsed: ParsedArgs): Promise<void> {
  const [action = "list", name, root] = parsed.positionals;
  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, undefined),
  );

  try {
    if (action === "list") {
      if (parsed.options.resetPaths) {
        throw new Error(
          "--reset-paths can only be used with --collections index",
        );
      }

      printCollectionList(await zvecGrep.collections.list(), parsed.options);
      return;
    }

    if (action === "info") {
      if (parsed.options.resetPaths) {
        throw new Error(
          "--reset-paths can only be used with --collections index",
        );
      }

      if (!name) {
        throw new Error("zg --collections info requires <name>");
      }

      const [info, status] = await Promise.all([
        zvecGrep.collections.info(name),
        zvecGrep.collections.status(name),
      ]);
      if (!info) {
        throw new Error(`Collection not found: ${name}`);
      }

      printCollectionInfo(info, status, parsed.options);
      return;
    }

    if (action === "index") {
      if (!name) {
        throw new Error("zg --collections index requires <name>");
      }

      const explicitRoot = root !== undefined;
      const rootPath = indexRootPath(root ?? process.cwd(), parsed.options);
      const rootPaths = explicitRoot ? rootPath : undefined;
      const progress = createIndexProgressReporter();
      try {
        const result = await zvecGrep.collections.index(name, rootPaths, {
          rebuild: parsed.options.rebuild,
          resetPaths: parsed.options.resetPaths,
          includePaths: parsed.options.includePaths,
          excludePaths: parsed.options.excludePaths,
          embeddingConcurrency: parsed.options.embeddingConcurrency,
          onProgress: progress.report,
        });
        progress.finish();
        const info = await zvecGrep.collections.info(name);
        printIndexResult(
          `Indexed collection ${name}`,
          result,
          parsed.options,
          info?.rootPaths,
        );
        persistExplicitGlobalConfig(parsed.options, info?.embedding?.provider);
      } catch (error) {
        progress.finish();
        throw error;
      }
      return;
    }

    if (action === "remove") {
      if (parsed.options.resetPaths) {
        throw new Error(
          "--reset-paths can only be used with --collections index",
        );
      }

      if (!name) {
        throw new Error("zg --collections remove requires <name>");
      }

      const removed = await zvecGrep.collections.remove(name);
      printCollectionRemoveResult(name, removed, parsed.options);
      return;
    }

    if (parsed.options.resetPaths) {
      throw new Error(
        "--reset-paths can only be used with --collections index",
      );
    }

    throw new Error(`Unknown collections action: ${action}`);
  } finally {
    await zvecGrep.close();
  }
}

async function runQuery(parsed: ParsedArgs): Promise<void> {
  const rgInput = parsed.options.rg ? normalizeRgInput(parsed) : undefined;
  const commandOptions = rgInput?.options ?? parsed.options;
  const queries = (rgInput?.queries ?? parsed.positionals)
    .map((query) => query.trim())
    .filter((query) => query.length > 0);
  const routes = parsed.options.routes ?? [];
  if (queries.length === 0 && routes.length === 0) {
    throw new Error(
      parsed.options.rg
        ? "zg --rg requires a pattern. Use --help for examples."
        : "zg query requires text or --fts/--vector routes. Use --help for examples.",
    );
  }

  const zvecGrep = await createZvecGrep(
    createServiceOptions(commandOptions, undefined),
  );
  const progress = createIndexProgressReporter();
  try {
    const result = await zvecGrep.context(
      contextOptions(commandOptions, queries, (progressEvent) => {
        if (progressEvent.phase !== "done") {
          progress.report(progressEvent);
        }
      }),
    );
    progress.finish();
    if (commandOptions.human) {
      printHumanContextResult(result, commandOptions);
    } else {
      printAgentContextResult(result, commandOptions);
    }
    for (const line of contextWarningLines(result)) {
      console.error(line);
    }

    if (commandOptions.debug) {
      printDebug(result, {
        trace: commandOptions.trace === true,
      });
    }
  } catch (error) {
    progress.finish();
    throw error;
  } finally {
    await zvecGrep.close();
  }
}

function contextOptions(
  options: CliOptions,
  queries: readonly string[],
  onAutoUpdateProgress?: (progress: IndexProgress) => void,
): ZvecGrepContextOptions {
  return {
    queries: queries.length > 0 ? queries : undefined,
    rg: options.rg,
    rgOptions: options.rgOptions,
    rgPaths: options.rgPaths,
    routes: options.routes,
    collection: options.collection,
    limit: options.limit,
    fallback: "disabled",
    autoUpdate: !options.noAutoUpdate,
    onAutoUpdateProgress,
    trace: options.trace,
    preferSymbol: options.preferSymbol,
    includePaths: options.includePaths,
    excludePaths: options.excludePaths,
    modifiedAfter: options.modifiedAfter,
    modifiedBefore: options.modifiedBefore,
    symbolTypes: options.symbolTypes,
    embeddingConcurrency: options.embeddingConcurrency,
  };
}

function normalizeRgInput(parsed: ParsedArgs): {
  queries: string[];
  options: CliOptions;
} {
  const explicitPatterns = parsed.options.rgOptions?.patterns ?? [];
  const queries =
    explicitPatterns.length > 0
      ? explicitPatterns
      : parsed.positionals.slice(0, 1);
  const paths =
    explicitPatterns.length > 0
      ? parsed.positionals
      : parsed.positionals.slice(1);

  return {
    queries,
    options: {
      ...parsed.options,
      rgPaths: paths.length > 0 ? paths : undefined,
    },
  };
}

export function createServiceOptions(
  options: CliOptions,
  root: string | undefined,
): CreateZvecGrepOptions {
  const embedding = options.embedding ?? process.env.ZVEC_GREP_EMBEDDING;
  const apiKey =
    options.apiKey ??
    process.env.ZVEC_GREP_API_KEY ??
    process.env.DASHSCOPE_API_KEY ??
    process.env.QWEN_API_KEY;
  const endpoint = options.endpoint ?? process.env.ZVEC_GREP_ENDPOINT;

  return {
    root,
    home: options.home ?? process.env.ZVEC_GREP_HOME,
    embedding,
    apiKey,
    endpoint,
    modelCacheDir: options.modelCacheDir ?? process.env.ZVEC_GREP_MODEL_CACHE,
    llamaGpu:
      options.llamaGpu ?? parseEnvLlamaGpu(process.env.ZVEC_GREP_LLAMA_GPU),
    embeddingParallelism:
      options.embeddingParallelism ??
      parseEnvPositiveInteger(process.env.ZVEC_GREP_EMBED_PARALLELISM),
  };
}

function persistExplicitGlobalConfig(
  options: CliOptions,
  indexedProvider: string | undefined,
): void {
  if (!updateGlobalConfigFromExplicitOptions(options, indexedProvider)) {
    return;
  }

  console.log(`Global config: ${globalConfigPath()}`);
}

function parseEnvLlamaGpu(
  value: string | undefined,
): CreateZvecGrepOptions["llamaGpu"] | undefined {
  const normalized = value?.trim().toLowerCase() ?? "";
  if (!normalized) {
    return undefined;
  }

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

  return undefined;
}

function parseEnvPositiveInteger(
  value: string | undefined,
): number | undefined {
  const normalized = value?.trim() ?? "";
  if (!normalized) {
    return undefined;
  }

  const parsed = Number.parseInt(normalized, 10);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : undefined;
}

function resolveCodexHome(): string {
  return resolve(process.env.CODEX_HOME ?? resolve(homedir(), ".codex"));
}

async function writeMarkedFile(options: {
  path: string;
  startMarker: string;
  endMarker: string;
  block: string;
  force: boolean;
  hasConflict?: (existing: string) => boolean;
  conflictMessage?: string;
  removeConflict?: (existing: string) => string;
}): Promise<void> {
  let existing = await readTextFileIfExists(options.path);
  const next = replaceMarkedBlock(
    existing,
    options.startMarker,
    options.endMarker,
    options.block,
  );

  if (next === null) {
    existing = removeOrphanedMarkers(
      existing,
      options.startMarker,
      options.endMarker,
    );
  }

  if (next === null && options.hasConflict?.(existing)) {
    if (!options.force) {
      throw new Error(
        options.conflictMessage ??
          `Existing unmanaged configuration found in ${options.path}`,
      );
    }
    existing = options.removeConflict
      ? options.removeConflict(existing)
      : existing;
  }

  await writeTextFileAtomic(
    options.path,
    next ?? appendMarkedBlock(existing, options.block),
  );
}

async function writeTextFileAtomic(
  path: string,
  content: string,
): Promise<void> {
  const targetPath = await resolveAtomicWriteTarget(path);
  await mkdir(dirname(targetPath), { recursive: true });

  const mode = await fileModeIfExists(targetPath);
  const temporaryPath = `${targetPath}.${process.pid}.${randomUUID()}.tmp`;
  let temporaryFile;

  try {
    temporaryFile = await open(temporaryPath, "wx", mode);
    await temporaryFile.writeFile(content, "utf8");
    await temporaryFile.sync();
    await temporaryFile.close();
    temporaryFile = undefined;

    if (mode !== undefined) {
      await chmod(temporaryPath, mode);
    }
    await rename(temporaryPath, targetPath);
  } catch (error) {
    await temporaryFile?.close().catch(() => undefined);
    await unlink(temporaryPath).catch(() => undefined);
    throw error;
  }
}

async function resolveAtomicWriteTarget(
  path: string,
  visited = new Set<string>(),
): Promise<string> {
  const absolutePath = resolve(path);
  if (visited.has(absolutePath)) {
    throw new Error(
      `Cannot atomically write through circular symbolic link: ${path}`,
    );
  }

  try {
    const file = await lstat(absolutePath);
    if (!file.isSymbolicLink()) {
      return absolutePath;
    }

    visited.add(absolutePath);
    const link = await readlink(absolutePath);
    return resolveAtomicWriteTarget(
      resolve(dirname(absolutePath), link),
      visited,
    );
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") {
      return absolutePath;
    }
    throw error;
  }
}

async function fileModeIfExists(path: string): Promise<number | undefined> {
  try {
    return (await stat(path)).mode & 0o777;
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") {
      return undefined;
    }
    throw error;
  }
}

async function readTextFileIfExists(path: string): Promise<string> {
  try {
    return await readFile(path, "utf8");
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") {
      return "";
    }
    throw error;
  }
}

function isNodeError(error: unknown): error is NodeJS.ErrnoException {
  return typeof error === "object" && error !== null && "code" in error;
}

function replaceMarkedBlock(
  existing: string,
  startMarker: string,
  endMarker: string,
  block: string,
): string | null {
  const lines = existing.split(/\r?\n/);
  const markerLines = new Set<number>();
  const ranges: Array<{ start: number; end: number }> = [];
  let pendingStart: number | undefined;

  for (const [index, line] of lines.entries()) {
    const trimmed = line.trim();
    if (trimmed === startMarker) {
      markerLines.add(index);
      pendingStart = index;
      continue;
    }
    if (trimmed === endMarker) {
      markerLines.add(index);
      if (pendingStart !== undefined) {
        ranges.push({ start: pendingStart, end: index });
        pendingStart = undefined;
      }
    }
  }

  const first = ranges[0];
  if (!first) {
    return null;
  }

  const before = retainedMarkedLines(lines, 0, first.start, markerLines, [])
    .join("\n")
    .trimEnd();
  const after = retainedMarkedLines(
    lines,
    first.end + 1,
    lines.length,
    markerLines,
    ranges.slice(1),
  )
    .join("\n")
    .trim();
  return (
    [before, block.trim(), after.trim()]
      .filter((part) => part.length > 0)
      .join("\n\n") + "\n"
  );
}

function retainedMarkedLines(
  lines: readonly string[],
  start: number,
  end: number,
  markerLines: ReadonlySet<number>,
  removedRanges: readonly { start: number; end: number }[],
): string[] {
  const retained: string[] = [];
  for (let index = start; index < end; index += 1) {
    if (markerLines.has(index)) {
      continue;
    }
    if (
      removedRanges.some((range) => index >= range.start && index <= range.end)
    ) {
      continue;
    }
    retained.push(lines[index]!);
  }
  return retained;
}

function removeOrphanedMarkers(
  existing: string,
  startMarker: string,
  endMarker: string,
): string {
  return (
    existing
      .split(/\r?\n/)
      .filter((line) => {
        const trimmed = line.trim();
        return trimmed !== startMarker && trimmed !== endMarker;
      })
      .join("\n")
      .trimEnd() + "\n"
  );
}

function appendMarkedBlock(existing: string, block: string): string {
  const prefix = existing.trimEnd();
  return `${prefix.length > 0 ? `${prefix}\n\n` : ""}${block.trim()}\n`;
}

function hasCodexMcpServerConfig(existing: string): boolean {
  return existing.split(/\r?\n/).some((line) => {
    const tableName = tomlTableName(line);
    return tableName !== undefined && isCodexMcpServerTableName(tableName);
  });
}

function removeCodexMcpServerConfig(existing: string): string {
  const lines = existing.split(/\r?\n/);
  const kept: string[] = [];
  let skipping = false;

  for (const line of lines) {
    const tableName = tomlTableName(line);
    if (tableName !== undefined) {
      skipping = isCodexMcpServerTableName(tableName);
    }

    if (!skipping) {
      kept.push(line);
    }
  }

  return kept.join("\n").trimEnd() + "\n";
}

function tomlTableName(line: string): string | undefined {
  return line.match(/^\s*\[([^\]]+)\]\s*(?:#.*)?$/)?.[1]?.trim();
}

function isCodexMcpServerTableName(tableName: string): boolean {
  return /^(?:mcp_servers|"mcp_servers"|'mcp_servers')\s*\.\s*(?:zvec_grep|"zvec_grep"|'zvec_grep')(?:\s*\.|$)/.test(
    tableName,
  );
}

function codexConfigBlock(mcpToolTimeoutSeconds: number): string {
  return `${ZVEC_GREP_CONFIG_START}
[mcp_servers.zvec_grep]
command = "zg"
args = ["serve", "--mcp"]
env_vars = [
  "DASHSCOPE_API_KEY",
  "QWEN_API_KEY",
  "ZVEC_GREP_API_KEY",
  "ZVEC_GREP_EMBEDDING",
  "ZVEC_GREP_ENDPOINT",
  "ZVEC_GREP_HOME",
  "ZVEC_GREP_MODEL_CACHE",
  "ZVEC_GREP_LLAMA_GPU",
  "ZVEC_GREP_EMBED_PARALLELISM",
  "NODE_LLAMA_CPP_CMAKE_OPTION_GGML_OPENMP",
  "NODE_LLAMA_CPP_CMAKE_OPTION_GGML_NATIVE",
  "NODE_LLAMA_CPP_CMAKE_OPTION_GGML_CPU_ARM_ARCH"
]
startup_timeout_sec = 20
tool_timeout_sec = ${mcpToolTimeoutSeconds}
default_tools_approval_mode = "auto"
${ZVEC_GREP_CONFIG_END}`;
}

function codexAgentsBlock(): string {
  return `${ZVEC_GREP_AGENTS_START}
## zvec-grep

Use zvec-grep before grep, rg, or broad file reads when you need to understand or locate code.

- **MCP tools**: Use \`zvec_grep_search\` for indexed semantic/lexical code search and \`zvec_grep_rg\` for explicit no-index lexical search.
- **Indexing and status**: These are CLI-only operations. If an index is missing, use \`zvec_grep_rg\` and mention \`zg --index\` as the opt-in setup path; use \`zg --status\` when status inspection is needed.
- **Shell fallback**: If the MCP server is unavailable, use \`zg --status\`, \`zg "<query>"\`, and \`zg --rg "<pattern>"\`.

Prefer focused include/exclude filters, and exclude dependencies, generated output, caches, build artifacts, and logs unless the task is about those files.
${ZVEC_GREP_AGENTS_END}`;
}

function resolveIndexRoot(root: string | undefined): string {
  if (root !== undefined) {
    return root;
  }

  return findNearestAnonymousWorkspace(process.cwd())?.root ?? process.cwd();
}

function indexRootPath(path: string, options: CliOptions): RootPath {
  return {
    absolutePath: resolve(path),
    recursive: true,
    include: options.includePaths,
    exclude: options.excludePaths,
  };
}
