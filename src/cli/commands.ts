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
  getEmbeddingModelCatalogEntryByRef,
  type CreateZvecGrepOptions,
  type IndexProgress,
  type RootPath,
  type ZvecGrepContextResult,
  type ZvecGrepContextOptions,
  type ZvecGrepContextRoute,
} from "../index.js";
import {
  globalConfigPath,
  updateGlobalConfig,
  updateGlobalConfigFromExplicitOptions,
} from "../engine/config.js";
import { DaemonClient } from "../client/daemon-client.js";
import { resolveServerSearchPolicy } from "../client/search-policy.js";
import {
  resolveClientMode,
  resolveServerUrl,
  routeByMode,
} from "../client/mode-router.js";
import { serverStatus } from "../daemon/server-controller.js";
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
  printServerIndexInfo,
} from "./format/status.js";

type AgentInstaller = {
  id: string;
  label: string;
  description: string;
  install: (options: InstallAgentOptions) => Promise<InstallAgentResult>;
  uninstall: () => Promise<InstallAgentResult>;
};

type InstallAgentOptions = {
  force: boolean;
  mcpToolTimeoutSeconds: number;
  mcpTokenEnv?: string;
};

type InstallAgentResult = {
  files: string[];
};

const AGENT_INSTALLERS: readonly AgentInstaller[] = [
  {
    id: "codex",
    label: "Codex",
    description: "configure the zvec-grep MCP server",
    install: installCodexIntegration,
    uninstall: uninstallCodexIntegration,
  },
  {
    id: "claude",
    label: "Claude Code",
    description: "configure the zvec-grep MCP server",
    install: installClaudeCodeIntegration,
    uninstall: uninstallClaudeCodeIntegration,
  },
  {
    id: "opencode",
    label: "OpenCode",
    description: "configure the zvec-grep MCP server",
    install: installOpenCodeIntegration,
    uninstall: uninstallOpenCodeIntegration,
  },
  {
    id: "cursor",
    label: "Cursor",
    description: "configure the zvec-grep MCP server",
    install: installCursorIntegration,
    uninstall: uninstallCursorIntegration,
  },
];

const ZVEC_GREP_CONFIG_START = "# ZVEC_GREP_START";
const ZVEC_GREP_CONFIG_END = "# ZVEC_GREP_END";
const ZVEC_GREP_AGENTS_START = "<!-- ZVEC_GREP_START -->";
const ZVEC_GREP_AGENTS_END = "<!-- ZVEC_GREP_END -->";
const DEFAULT_MCP_TOOL_TIMEOUT_SECONDS = 600;

export async function runParsedCommand(parsed: ParsedArgs): Promise<void> {
  switch (parsed.command) {
    case "query":
      await runQuery(parsed);
      return;
    case "index":
      await runIndex(parsed);
      return;
    case "status":
      await runStatus(parsed);
      return;
    case "collections":
      await runCollections(parsed);
      return;
    case "install":
      await runInstall(parsed);
      return;
    case "uninstall":
      await runUninstall(parsed);
      return;
    case "config":
      runConfig(parsed);
      return;
    case "server":
      await runServer(parsed);
      return;
    case "help":
    case "version":
      throw new Error(`${parsed.command} must be handled before dispatch`);
  }
}

function runConfig(parsed: ParsedArgs): void {
  if (parsed.options.configAction !== "model-set") {
    throw new Error("zg config requires model set");
  }
  if (parsed.positionals.length !== 1) {
    throw new Error(
      "zg config model set requires exactly one local embedding reference",
    );
  }
  const reference = parsed.positionals[0]!;
  const separator = reference.indexOf("/");
  const provider = separator > 0 ? reference.slice(0, separator) : "";
  const model = separator > 0 ? reference.slice(separator + 1) : "";
  const catalogEntry = getEmbeddingModelCatalogEntryByRef({ provider, model });
  if (provider !== "local") {
    throw new Error("zg config model set only supports local embedding models");
  }
  if (!catalogEntry || catalogEntry.provider !== "local") {
    throw new Error(`Unsupported local embedding model: ${reference}`);
  }
  if (
    parsed.options.llamaGpu === undefined &&
    parsed.options.embeddingParallelism === undefined
  ) {
    throw new Error(
      "zg config model set requires --llama-gpu, --gpu, --no-gpu, or --embedding-parallelism",
    );
  }
  updateGlobalConfig({
    models: {
      [reference]: {
        ...(parsed.options.llamaGpu !== undefined
          ? { llamaGpu: parsed.options.llamaGpu }
          : {}),
        ...(parsed.options.embeddingParallelism !== undefined
          ? { embeddingParallelism: parsed.options.embeddingParallelism }
          : {}),
      },
    },
  });
  console.log(`Model config: ${reference}`);
  console.log(`Global config: ${globalConfigPath()}`);
}

async function runInstall(parsed: ParsedArgs): Promise<void> {
  const installers = await resolveInstallers(parsed, "install");
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
      mcpTokenEnv: parsed.options.installMcpTokenEnv,
    });
    console.log(`Installed ${installer.label}:`);
    for (const file of result.files) {
      console.log(`  ${file}`);
    }
  }

  console.log(
    "Restart the selected agent or start a new session to pick up the integration.",
  );
  console.log(`zvec-grep MCP endpoint: ${resolveServerUrl()}`);
}

async function runUninstall(parsed: ParsedArgs): Promise<void> {
  const installers = await resolveInstallers(parsed, "uninstall");
  if (installers.length === 0) {
    console.log("No agents selected.");
    return;
  }

  console.log(
    `Removing zvec-grep from: ${installers.map((installer) => installer.label).join(", ")}`,
  );
  for (const installer of installers) {
    const result = await installer.uninstall();
    console.log(`Removed ${installer.label} integration:`);
    for (const file of result.files) {
      console.log(`  ${file}`);
    }
  }

  console.log(
    "Restart the selected agent or start a new session to apply the change.",
  );
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
    block: codexConfigBlock(options.mcpToolTimeoutSeconds, options.mcpTokenEnv),
    force: options.force,
    hasConflict: hasCodexMcpServerConfig,
    conflictMessage: `Existing [mcp_servers.zvec_grep] found in ${configPath}. Re-run with --force after removing or moving that table into the zvec-grep managed block.`,
    removeConflict: removeCodexMcpServerConfig,
  });

  // Remove guidance written by older releases. Integrations are MCP-only now.
  await removeMarkedFile({
    path: agentsPath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
  });

  return { files: [configPath] };
}

async function uninstallCodexIntegration(): Promise<InstallAgentResult> {
  const codexHome = resolveCodexHome();
  const configPath = resolve(codexHome, "config.toml");
  const agentsPath = resolve(codexHome, "AGENTS.md");

  await removeMarkedFile({
    path: configPath,
    startMarker: ZVEC_GREP_CONFIG_START,
    endMarker: ZVEC_GREP_CONFIG_END,
  });
  await removeMarkedFile({
    path: agentsPath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
  });

  return { files: [configPath, agentsPath] };
}

async function installClaudeCodeIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const configPath = resolveClaudeCodeConfigPath();
  await installJsonMcpServer({
    path: configPath,
    containerKey: "mcpServers",
    server: {
      type: "http",
      url: resolveServerUrl(),
      ...(options.mcpTokenEnv
        ? {
            headers: {
              Authorization: `Bearer \${${options.mcpTokenEnv}}`,
            },
          }
        : {}),
    },
    force: options.force,
    label: "Claude Code",
  });
  return { files: [configPath] };
}

async function uninstallClaudeCodeIntegration(): Promise<InstallAgentResult> {
  const configPath = resolveClaudeCodeConfigPath();
  await uninstallJsonMcpServer(configPath, "mcpServers");
  return { files: [configPath] };
}

async function installOpenCodeIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const configPath = resolveOpenCodeConfigPath();
  await installJsonMcpServer({
    path: configPath,
    containerKey: "mcp",
    server: {
      type: "remote",
      url: resolveServerUrl(),
      enabled: true,
      timeout: options.mcpToolTimeoutSeconds * 1_000,
      oauth: false,
      ...(options.mcpTokenEnv
        ? {
            headers: {
              Authorization: `Bearer {env:${options.mcpTokenEnv}}`,
            },
          }
        : {}),
    },
    force: options.force,
    label: "OpenCode",
  });
  return { files: [configPath] };
}

async function uninstallOpenCodeIntegration(): Promise<InstallAgentResult> {
  const configPath = resolveOpenCodeConfigPath();
  await uninstallJsonMcpServer(configPath, "mcp");
  return { files: [configPath] };
}

async function installCursorIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const configPath = resolveCursorConfigPath();
  await installJsonMcpServer({
    path: configPath,
    containerKey: "mcpServers",
    server: {
      url: resolveServerUrl(),
      ...(options.mcpTokenEnv
        ? {
            headers: {
              Authorization: `Bearer \${${options.mcpTokenEnv}}`,
            },
          }
        : {}),
    },
    force: options.force,
    label: "Cursor",
  });
  return { files: [configPath] };
}

async function uninstallCursorIntegration(): Promise<InstallAgentResult> {
  const configPath = resolveCursorConfigPath();
  await uninstallJsonMcpServer(configPath, "mcpServers");
  return { files: [configPath] };
}

async function resolveInstallers(
  parsed: ParsedArgs,
  action: "install" | "uninstall",
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

  return promptInstallers(action);
}

async function promptInstallers(
  action: "install" | "uninstall",
): Promise<AgentInstaller[]> {
  console.log(
    `Select agents whose zvec-grep integration should be ${action === "install" ? "installed" : "removed"}:`,
  );
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
    const input = token.toLowerCase();
    const lower = input === "cc" || input === "claude-code" ? "claude" : input;
    if (lower === "none") {
      return [];
    }

    if (lower === "auto" || lower === "all") {
      for (const installer of AGENT_INSTALLERS) {
        selected.set(installer.id, installer);
      }
      continue;
    }

    const numbered = /^\d+$/.test(lower) ? Number(lower) : Number.NaN;
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
    throw new Error("zg index accepts at most one root path");
  }

  if (parsed.options.drop) {
    await runDropIndex(parsed, rootPath.absolutePath);
    return;
  }

  const mode = resolveClientMode(parsed.options.mode);
  await routeByMode({
    mode,
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const result = await daemonClient(parsed.options).callTool(
        "zvec_grep_index",
        {
          root: rootPath.absolutePath,
          embedding: parsed.options.embedding,
          rebuild: parsed.options.rebuild,
          resetPaths: parsed.options.resetPaths,
          globs: parsed.options.globs,
          insensitiveGlobs: parsed.options.insensitiveGlobs,
          fileTypes: parsed.options.fileTypes,
          excludedFileTypes: parsed.options.excludedFileTypes,
          hidden: parsed.options.hidden,
          noIgnore: parsed.options.noIgnore,
          ignoreFiles: parsed.options.ignoreFiles,
          maxDepth: parsed.options.maxDepth,
          maxFileSizeBytes: parsed.options.maxFileSizeBytes,
          follow: parsed.options.follow,
          embeddingConcurrency: parsed.options.embeddingConcurrency,
          wait: true,
        },
      );
      console.log(
        `Indexed anonymous workspace: ${String(result.state ?? "submitted")}`,
      );
      console.log(`Root: ${String(result.root ?? rootPath.absolutePath)}`);
      console.log(`Job: ${String(result.job_id ?? "unknown")}`);
    },
    direct: () => runDirectIndex(parsed, rootPath, explicitRoot),
  });
}

async function runDirectIndex(
  parsed: ParsedArgs,
  rootPath: RootPath,
  explicitRoot: boolean,
): Promise<void> {
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
      globs: parsed.options.globs,
      insensitiveGlobs: parsed.options.insensitiveGlobs,
      fileTypes: parsed.options.fileTypes,
      excludedFileTypes: parsed.options.excludedFileTypes,
      hidden: parsed.options.hidden,
      noIgnore: parsed.options.noIgnore,
      ignoreFiles: parsed.options.ignoreFiles,
      maxDepth: parsed.options.maxDepth,
      maxFileSizeBytes: parsed.options.maxFileSizeBytes,
      follow: parsed.options.follow,
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
      embeddingReference(info.collection?.embedding),
    );
  } catch (error) {
    progress.finish();
    throw error;
  } finally {
    await zvecGrep.close();
  }
}

async function runDropIndex(parsed: ParsedArgs, root: string): Promise<void> {
  await assertDirectOnlyOperation(parsed.options, "zg index --drop");
  if (!(await confirmIndexDrop(root, parsed.options.yes === true))) {
    console.log("Index drop cancelled.");
    return;
  }

  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, root),
  );
  try {
    const removed = await zvecGrep.dropIndex({ root });
    console.log(
      removed ? `Dropped index for ${root}` : `No index found for ${root}`,
    );
  } finally {
    await zvecGrep.close();
  }
}

async function confirmIndexDrop(
  root: string,
  accepted: boolean,
): Promise<boolean> {
  if (accepted) {
    return true;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error(
      "zg index --drop requires --yes in a non-interactive shell",
    );
  }

  const readline = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  try {
    const answer = await readline.question(
      `Drop the index for ${root}? [y/N] `,
    );
    return ["y", "yes"].includes(answer.trim().toLowerCase());
  } finally {
    readline.close();
  }
}

async function runServer(parsed: ParsedArgs): Promise<void> {
  if (parsed.positionals.length > 0) {
    throw new Error(
      `zg server ${parsed.options.serverAction} does not accept positional arguments`,
    );
  }
  const { readPackageVersion } = await import("./version.js");
  if (parsed.options.serverAction === "on") {
    const { startServer } = await import("../daemon/server-controller.js");
    const status = await startServer({
      cliPath: process.argv[1]!,
      listen: parsed.options.listen,
      tokenFile: parsed.options.serverTokenFile,
      home: parsed.options.home,
    });
    printServerControlStatus(status);
    return;
  }
  if (parsed.options.serverAction === "off") {
    const { stopServer } = await import("../daemon/server-controller.js");
    printServerControlStatus(
      await stopServer(
        parsed.options.home,
        30_000,
        parsed.options.serverTokenFile,
      ),
    );
    return;
  }
  if (parsed.options.serverAction === "status") {
    printServerControlStatus(await serverStatus(parsed.options.home));
    return;
  }

  const { runDaemonForeground } = await import("../daemon/runtime.js");
  await runDaemonForeground({
    version: readPackageVersion(),
    listen: parsed.options.listen,
    tokenFile: parsed.options.serverTokenFile,
    home: parsed.options.home,
    serviceOptions: createServiceOptions(parsed.options, process.cwd()),
  });
}

async function runStatus(parsed: ParsedArgs): Promise<void> {
  const root = parsed.positionals[0] ?? process.cwd();
  if (parsed.positionals.length > 1) {
    throw new Error("zg status accepts at most one root path");
  }

  const mode = resolveClientMode(parsed.options.mode);
  await routeByMode({
    mode,
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const result = await daemonClient(parsed.options).callTool(
        "zvec_grep_index_status",
        { root: resolve(root) },
      );
      printServerIndexInfo(
        result as Parameters<typeof printServerIndexInfo>[0],
        parsed.options,
      );
    },
    direct: async () => {
      const zvecGrep = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        printAnonymousInfo(await zvecGrep.info({ root }), parsed.options);
      } finally {
        await zvecGrep.close();
      }
    },
  });
}

async function runCollections(parsed: ParsedArgs): Promise<void> {
  assertDirectOnlyMode(parsed.options, "--collections");
  const [action = "list", name, root] = parsed.positionals;
  validateCollectionArguments(action, parsed.positionals);
  const indexOption = collectionIndexOption(parsed.options);
  if (action !== "index" && indexOption) {
    throw new Error(
      `${indexOption} can only be used with zg collections index`,
    );
  }
  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, undefined),
  );

  try {
    if (action === "list") {
      if (parsed.options.resetPaths) {
        throw new Error(
          "--reset-paths can only be used with zg collections index",
        );
      }

      printCollectionList(await zvecGrep.collections.list(), parsed.options);
      return;
    }

    if (action === "info") {
      if (parsed.options.resetPaths) {
        throw new Error(
          "--reset-paths can only be used with zg collections index",
        );
      }

      if (!name) {
        throw new Error("zg collections info requires <name>");
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
        throw new Error("zg collections index requires <name>");
      }

      const explicitRoot = root !== undefined;
      const rootPath = indexRootPath(root ?? process.cwd(), parsed.options);
      const rootPaths = explicitRoot ? rootPath : undefined;
      const progress = createIndexProgressReporter();
      try {
        const result = await zvecGrep.collections.index(name, rootPaths, {
          rebuild: parsed.options.rebuild,
          resetPaths: parsed.options.resetPaths,
          globs: parsed.options.globs,
          insensitiveGlobs: parsed.options.insensitiveGlobs,
          fileTypes: parsed.options.fileTypes,
          excludedFileTypes: parsed.options.excludedFileTypes,
          hidden: parsed.options.hidden,
          noIgnore: parsed.options.noIgnore,
          ignoreFiles: parsed.options.ignoreFiles,
          maxDepth: parsed.options.maxDepth,
          maxFileSizeBytes: parsed.options.maxFileSizeBytes,
          follow: parsed.options.follow,
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
        persistExplicitGlobalConfig(
          parsed.options,
          embeddingReference(info?.embedding),
        );
      } catch (error) {
        progress.finish();
        throw error;
      }
      return;
    }

    if (action === "remove") {
      if (parsed.options.resetPaths) {
        throw new Error(
          "--reset-paths can only be used with zg collections index",
        );
      }

      if (!name) {
        throw new Error("zg collections remove requires <name>");
      }

      const removed = await zvecGrep.collections.remove(name);
      printCollectionRemoveResult(name, removed, parsed.options);
      return;
    }

    if (parsed.options.resetPaths) {
      throw new Error(
        "--reset-paths can only be used with zg collections index",
      );
    }

    throw new Error(`Unknown collections action: ${action}`);
  } finally {
    await zvecGrep.close();
  }
}

function validateCollectionArguments(
  action: string,
  positionals: readonly string[],
): void {
  if (action === "list") {
    if (positionals.length > 1) {
      throw new Error("zg collections list does not accept arguments");
    }
    return;
  }

  if (action === "info" || action === "remove") {
    if (positionals.length > 2) {
      throw new Error(`zg collections ${action} accepts only <name>`);
    }
    return;
  }

  if (action === "index") {
    if (positionals.length > 3) {
      throw new Error("zg collections index accepts only <name> and [root]");
    }
    return;
  }

  throw new Error(`Unknown collections action: ${action}`);
}

function collectionIndexOption(options: CliOptions): string | undefined {
  const candidates: readonly (readonly [unknown, string])[] = [
    [options.rebuild, "--rebuild"],
    [options.resetPaths, "--reset-paths"],
    [options.embedding, "--embedding"],
    [options.modelCacheDir, "--model-cache"],
    [options.llamaGpu, "--llama-gpu"],
    [options.embeddingParallelism, "--embedding-parallelism"],
    [options.apiKey, "--api-key"],
    [options.endpoint, "--endpoint"],
    [options.embeddingConcurrency, "--embedding-concurrency"],
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
  ];
  return candidates.find(([value]) =>
    Array.isArray(value) ? value.length > 0 : value !== undefined,
  )?.[1];
}

async function runQuery(parsed: ParsedArgs): Promise<void> {
  const rgInput = parsed.options.rg ? normalizeRgInput(parsed) : undefined;
  const commandOptions = rgInput?.options ?? parsed.options;
  const queries = (
    rgInput?.queries ?? [
      ...parsed.positionals,
      ...(parsed.options.hybridQueries ?? []),
    ]
  )
    .map((query) => query.trim())
    .filter((query) => query.length > 0);
  const routes = parsed.options.routes ?? [];
  if (
    queries.length === 0 &&
    routes.length === 0 &&
    (parsed.options.rgOptions?.patternFiles?.length ?? 0) === 0
  ) {
    throw new Error(
      parsed.options.rg
        ? "zg query --rg requires a pattern. Use zg help query for examples."
        : "zg query requires text or --hybrid/--fts/--vector routes. Use zg help query for examples.",
    );
  }
  if (!commandOptions.rg && !commandOptions.collection) {
    const mode = resolveClientMode(commandOptions.mode);
    if (mode !== "direct") {
      await routeByMode({
        mode,
        serverAvailable: () => daemonIsReady(commandOptions.home),
        server: () => runServerQuery(commandOptions, queries, routes),
        direct: () => runDirectQuery(commandOptions, queries),
      });
      return;
    }
  }
  await runDirectQuery(commandOptions, queries);
}

async function runDirectQuery(
  commandOptions: CliOptions,
  queries: readonly string[],
): Promise<void> {
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

async function runServerQuery(
  options: CliOptions,
  queries: readonly string[],
  routes: readonly ZvecGrepContextRoute[],
): Promise<void> {
  const searchPolicy = resolveServerSearchPolicy(options);
  const fts = routes
    .filter((route) => route.mode === "fts")
    .map((route) => route.query);
  const vector = routes
    .filter((route) => route.mode === "vector")
    .map((route) => route.query);
  const response = await daemonClient(options).callTool("zvec_grep_search", {
    root: resolve(process.cwd()),
    queries: queries.length ? queries : undefined,
    fts: fts.length ? fts : undefined,
    vector: vector.length ? vector : undefined,
    fuse: options.fuse,
    limit: options.limit,
    trace: options.trace,
    preferSymbol: options.preferSymbol,
    symbolTypes: options.symbolTypes,
    globs: options.globs,
    insensitiveGlobs: options.insensitiveGlobs,
    fileTypes: options.fileTypes,
    excludedFileTypes: options.excludedFileTypes,
    hidden: options.hidden,
    noIgnore: options.noIgnore,
    ignoreFiles: options.ignoreFiles,
    maxDepth: options.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes,
    follow: options.follow,
    embeddingConcurrency: options.embeddingConcurrency,
    modifiedAfter: options.modifiedAfter,
    modifiedBefore: options.modifiedBefore,
    freshness: searchPolicy.freshness,
    autoUpdate: searchPolicy.autoUpdate,
  });
  const result = response.result as ZvecGrepContextResult;
  if (options.human) printHumanContextResult(result, options);
  else printAgentContextResult(result, options);
  if (response.freshness === "possibly_stale") {
    console.error("status: possibly_stale");
    const indexing = response.indexing as
      { state?: string; completed?: number; total?: number } | undefined;
    if (indexing?.state) {
      const progress =
        indexing.completed === undefined || indexing.total === undefined
          ? ""
          : ` (${indexing.completed}/${indexing.total})`;
      console.error(`indexing: ${indexing.state}${progress}`);
    }
  }
}

function daemonClient(options: CliOptions): DaemonClient {
  return new DaemonClient({
    serverUrl: resolveServerUrl(),
    home: options.home,
    tokenFile: options.serverTokenFile,
  });
}

async function daemonIsReady(home?: string): Promise<boolean> {
  return (await serverStatus(home)).ready;
}

function printServerControlStatus(
  status: Awaited<ReturnType<typeof serverStatus>>,
): void {
  console.log(
    `Server: ${status.running ? (status.ready ? "ready" : "starting") : "stopped"}`,
  );
  if (status.pid) console.log(`PID: ${status.pid}`);
  if (status.serverUrl) console.log(`URL: ${status.serverUrl}`);
}

function assertDirectOnlyMode(options: CliOptions, command: string): void {
  if (resolveClientMode(options.mode) === "server") {
    throw new Error(
      `${command} is Direct-only; use --mode direct after stopping the daemon`,
    );
  }
}

async function assertDirectOnlyOperation(
  options: CliOptions,
  command: string,
): Promise<void> {
  const mode = resolveClientMode(options.mode);
  if (
    mode === "server" ||
    (mode === "auto" && (await daemonIsReady(options.home)))
  ) {
    throw new Error(
      `${command} is Direct-only; use --mode direct after stopping the daemon`,
    );
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
    fuse: options.fuse,
    collection: options.collection,
    limit: options.limit,
    autoUpdate: !options.noAutoUpdate,
    onAutoUpdateProgress,
    trace: options.trace,
    preferSymbol: options.preferSymbol,
    globs: options.globs,
    insensitiveGlobs: options.insensitiveGlobs,
    fileTypes: options.fileTypes,
    excludedFileTypes: options.excludedFileTypes,
    hidden: options.hidden,
    noIgnore: options.noIgnore,
    ignoreFiles: options.ignoreFiles,
    maxDepth: options.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes,
    follow: options.follow,
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
  const hasPatternFiles =
    (parsed.options.rgOptions?.patternFiles?.length ?? 0) > 0;
  const queries =
    explicitPatterns.length > 0 || hasPatternFiles
      ? explicitPatterns
      : parsed.positionals.slice(0, 1);
  const paths =
    explicitPatterns.length > 0 || hasPatternFiles
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
  indexedEmbedding: string | undefined,
): void {
  if (!updateGlobalConfigFromExplicitOptions(options, indexedEmbedding)) {
    return;
  }

  console.log(`Global config: ${globalConfigPath()}`);
}

function embeddingReference(
  embedding: { provider: string; model: string } | null | undefined,
): string | undefined {
  return embedding ? `${embedding.provider}/${embedding.model}` : undefined;
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
  if (!/^\d+$/.test(normalized)) {
    return undefined;
  }

  const parsed = Number(normalized);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : undefined;
}

function resolveCodexHome(): string {
  return resolve(process.env.CODEX_HOME ?? resolve(homedir(), ".codex"));
}

function resolveClaudeCodeConfigPath(): string {
  return process.env.CLAUDE_CONFIG_DIR
    ? resolve(process.env.CLAUDE_CONFIG_DIR, ".claude.json")
    : resolve(homedir(), ".claude.json");
}

function resolveOpenCodeConfigPath(): string {
  return resolve(
    process.env.OPENCODE_CONFIG ??
      resolve(homedir(), ".config", "opencode", "opencode.json"),
  );
}

function resolveCursorConfigPath(): string {
  return resolve(
    process.env.CURSOR_CONFIG_DIR ?? resolve(homedir(), ".cursor"),
    "mcp.json",
  );
}

async function installJsonMcpServer(options: {
  path: string;
  containerKey: "mcp" | "mcpServers";
  server: Record<string, unknown>;
  force: boolean;
  label: string;
}): Promise<void> {
  const config = await readJsonObject(options.path);
  const existingContainer = config[options.containerKey];
  if (existingContainer !== undefined && !isJsonObject(existingContainer)) {
    throw new Error(
      `Expected ${options.containerKey} in ${options.path} to be a JSON object`,
    );
  }

  const container = { ...(existingContainer ?? {}) } as Record<string, unknown>;
  const existingServer = container.zvec_grep;
  if (
    existingServer !== undefined &&
    !isManagedJsonMcpServer(existingServer) &&
    !options.force
  ) {
    throw new Error(
      `Existing unmanaged zvec_grep MCP server found in ${options.path}. Re-run with --force to replace it for ${options.label}.`,
    );
  }

  container.zvec_grep = options.server;
  config[options.containerKey] = container;
  await writeTextFileAtomic(
    options.path,
    `${JSON.stringify(config, null, 2)}\n`,
  );
}

async function uninstallJsonMcpServer(
  path: string,
  containerKey: "mcp" | "mcpServers",
): Promise<void> {
  const existing = await readTextFileIfExists(path);
  if (!existing) return;

  const config = parseJsonObject(existing, path);
  const container = config[containerKey];
  if (!isJsonObject(container)) return;
  if (!isManagedJsonMcpServer(container.zvec_grep)) return;

  const nextContainer = { ...container };
  delete nextContainer.zvec_grep;
  if (Object.keys(nextContainer).length === 0) {
    delete config[containerKey];
  } else {
    config[containerKey] = nextContainer;
  }
  await writeTextFileAtomic(path, `${JSON.stringify(config, null, 2)}\n`);
}

async function readJsonObject(path: string): Promise<Record<string, unknown>> {
  const existing = await readTextFileIfExists(path);
  return existing ? parseJsonObject(existing, path) : {};
}

function parseJsonObject(value: string, path: string): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch (error) {
    throw new Error(
      `Cannot update ${path}: invalid JSON (${error instanceof Error ? error.message : String(error)})`,
      { cause: error },
    );
  }
  if (!isJsonObject(parsed)) {
    throw new Error(`Cannot update ${path}: expected a JSON object`);
  }
  return parsed;
}

function isJsonObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isManagedJsonMcpServer(value: unknown): boolean {
  return isJsonObject(value) && value.url === resolveServerUrl();
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

async function removeMarkedFile(options: {
  path: string;
  startMarker: string;
  endMarker: string;
}): Promise<void> {
  const existing = await readTextFileIfExists(options.path);
  if (!existing) {
    return;
  }

  const next =
    replaceMarkedBlock(existing, options.startMarker, options.endMarker, "") ??
    removeOrphanedMarkers(existing, options.startMarker, options.endMarker);
  if (next === existing) {
    return;
  }

  await writeTextFileAtomic(options.path, next);
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

function codexConfigBlock(
  mcpToolTimeoutSeconds: number,
  tokenEnv?: string,
): string {
  return `${ZVEC_GREP_CONFIG_START}
[mcp_servers.zvec_grep]
url = "${resolveServerUrl()}"
${
  tokenEnv
    ? `bearer_token_env_var = "${tokenEnv}"
`
    : ""
}tool_timeout_sec = ${mcpToolTimeoutSeconds}
default_tools_approval_mode = "auto"
${ZVEC_GREP_CONFIG_END}`;
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
    globs: options.globs,
    insensitiveGlobs: options.insensitiveGlobs,
    fileTypes: options.fileTypes,
    excludedFileTypes: options.excludedFileTypes,
    hidden: options.hidden,
    noIgnore: options.noIgnore,
    ignoreFiles: options.ignoreFiles,
    maxDepth: options.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes,
    follow: options.follow,
  };
}
