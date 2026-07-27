import { randomUUID } from "node:crypto";
import { constants as fileSystemConstants } from "node:fs";
import {
  access,
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
import { delimiter, dirname, resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import {
  createZvecGrep,
  getEmbeddingModelCatalogEntry,
  getEmbeddingModelCatalogEntryByRef,
  type CreateZvecGrepOptions,
  type IndexProgress,
  type RootPath,
  type ZvecGrepContextOptions,
  type ZvecGrepContextRoute,
  type ZvecGrep,
  type ZvecGrepInfoResult,
} from "../index.js";
import {
  globalConfigPath,
  updateGlobalConfig,
  updateGlobalConfigFromExplicitOptions,
} from "../engine/config.js";
import { DaemonClient } from "../client/daemon-client.js";
import {
  resolveDirectSearchPolicy,
  resolveServerSearchPolicy,
} from "../client/search-policy.js";
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
  printRemoteEmbeddingAuthorizationStatus,
  printServerIndexInfo,
} from "./format/status.js";
import {
  RemoteEmbeddingAuthorizationManager,
  RemoteEmbeddingAuthorizationStore,
  createRemoteEmbeddingTarget,
  formatRemoteEmbeddingAuthorizationPrompt,
  planRemoteIndexAuthorization,
  planRemoteSearchAuthorization,
  remoteEmbeddingDisclosureData,
  withRemoteEmbeddingOperationPermit,
  type RemoteEmbeddingAuthorizationPlan,
  type RemoteEmbeddingOperationPermit,
} from "../authorization/index.js";
import type { CollectionEmbeddingSchema } from "../engine/types.js";
import {
  indexCompletionFromStatus,
  indexStatusNeedsRefresh,
} from "../engine/index-status.js";
import type { NormalizedSearchInput } from "../mcp/input-normalization.js";
import { indexProgressFromMessage } from "../index-progress.js";

type AgentInstaller = {
  id: string;
  label: string;
  executable: string;
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
    id: "claude",
    label: "Claude Code",
    executable: "claude",
    install: installClaudeIntegration,
    uninstall: uninstallClaudeIntegration,
  },
  {
    id: "codex",
    label: "Codex",
    executable: "codex",
    install: installCodexIntegration,
    uninstall: uninstallCodexIntegration,
  },
  {
    id: "opencode",
    label: "OpenCode",
    executable: "opencode",
    install: installOpenCodeIntegration,
    uninstall: uninstallOpenCodeIntegration,
  },
  {
    id: "cursor",
    label: "Cursor",
    executable: "cursor",
    install: installCursorIntegration,
    uninstall: uninstallCursorIntegration,
  },
];

const ZVEC_GREP_CONFIG_START = "# ZVEC_GREP_START";
const ZVEC_GREP_CONFIG_END = "# ZVEC_GREP_END";
const ZVEC_GREP_AGENTS_START = "<!-- ZVEC_GREP_START -->";
const ZVEC_GREP_AGENTS_END = "<!-- ZVEC_GREP_END -->";
const CLAUDE_MCP_PERMISSION = "mcp__zvec_grep__*";
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
    case "auth":
      await runAuth(parsed);
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
  printInstallHeader();
  const installers = await resolveInstallers(parsed, "install");
  if (installers.length === 0) {
    console.log("\nNo agent integrations selected.");
    return;
  }

  console.log("\nInstalling integrations\n");
  for (const installer of installers) {
    await installer.install({
      force: parsed.options.force === true,
      mcpToolTimeoutSeconds:
        parsed.options.installMcpToolTimeoutSeconds ??
        DEFAULT_MCP_TOOL_TIMEOUT_SECONDS,
      mcpTokenEnv: parsed.options.installMcpTokenEnv,
    });
    console.log(`  ${installSuccessMark()} ${installer.label}`);
    console.log("    MCP       configured");
    console.log("");
  }

  const server = await ensureInstalledServer();
  if (server.ready) {
    console.log(`  ${installSuccessMark()} Server`);
    console.log(`    ready at ${server.serverUrl ?? resolveServerUrl()}`);
  } else {
    console.log(`  ${installMutedMark()} Server`);
    console.log("    not started; run `zg server on`");
  }

  console.log("\nzvec-grep is ready\n");
  console.log(
    `  Agents       ${installers.map((installer) => installer.label).join(", ")}`,
  );
  console.log("  Remote data  Authorization requested on first remote use");
  console.log(
    "\nRestart the selected agents or start a new session to load the integration.",
  );
}

async function runUninstall(parsed: ParsedArgs): Promise<void> {
  printInstallHeader();
  const installers = await resolveInstallers(parsed, "uninstall");
  if (installers.length === 0) {
    console.log("\nNo agent integrations selected.");
    return;
  }

  console.log("\nRemoving integrations\n");
  for (const installer of installers) {
    await installer.uninstall();
    console.log(`  ${installSuccessMark()} ${installer.label}`);
    console.log("    integration removed");
  }

  console.log(
    "\nRestart the selected agents or start a new session to apply the change.",
  );
}

async function runAuth(parsed: ParsedArgs): Promise<void> {
  const requestedRoot = resolve(parsed.positionals[0] ?? process.cwd());
  const root =
    findNearestAnonymousWorkspace(requestedRoot)?.root ?? requestedRoot;
  const store = authorizationStore(parsed.options);
  if (parsed.options.authAction === "status") {
    const status = await store.status(root);
    printRemoteEmbeddingAuthorizationStatus(root, status, parsed.options);
    return;
  }
  if (parsed.options.authAction === "revoke") {
    const revoked = await store.revokeAll(root);
    console.log(
      revoked > 0
        ? `Revoked ${revoked} Remote Embedding Workspace grant(s).`
        : "No Remote Embedding Workspace grants found.",
    );
    return;
  }
  if (parsed.options.authAction !== "grant") {
    throw new Error("zg auth requires grant, status, or revoke");
  }

  const serviceOptions = createServiceOptions(parsed.options, root);
  const service = await createZvecGrep(serviceOptions);
  try {
    const info = await service.info({ root });
    const schema = resolveAuthorizationSchema(
      parsed.options.embedding ?? process.env.ZVEC_GREP_EMBEDDING,
      info.collection?.embedding,
    );
    if (!schema) {
      throw new Error(
        "No embedding model is available. Pass --embedding <remote/model> or build an index first.",
      );
    }
    if (schema.provider === "local") {
      throw new Error("Local embedding models do not require authorization.");
    }
    const target = await createRemoteEmbeddingTarget({
      roots: info.collection?.rootPaths.map((item) => item.absolutePath) ?? [
        root,
      ],
      provider: schema.provider,
      model: schema.model,
      serviceOptions,
    });
    const manager = new RemoteEmbeddingAuthorizationManager(store);
    const plan: RemoteEmbeddingAuthorizationPlan = {
      operation: "query_and_index",
      target,
      disclosure: { queryText: true, workspaceContent: "full" },
      reason: "index_create",
      grantPath: store.grantPath(target),
    };
    await manager.grant(plan, "workspace");
    printRemoteEmbeddingAuthorizationStatus(
      root,
      await store.status(root),
      parsed.options,
    );
  } finally {
    await service.close();
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
    block: codexConfigBlock(options.mcpToolTimeoutSeconds, options.mcpTokenEnv),
    force: options.force,
    hasConflict: hasCodexMcpServerConfig,
    conflictMessage: `Existing [mcp_servers.zvec_grep] found in ${configPath}. Re-run with --force after removing or moving that table into the zvec-grep managed block.`,
    removeConflict: removeCodexMcpServerConfig,
  });

  await writeMarkedFile({
    path: agentsPath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
    block: agentGuidanceBlock(),
    force: true,
  });

  return { files: [configPath, agentsPath] };
}

async function installClaudeIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const configDirectory = resolveClaudeConfigDirectory();
  const mcpConfigPath = resolveClaudeMcpConfigPath();
  const settingsPath = resolve(configDirectory, "settings.json");
  const guidancePath = resolve(configDirectory, "CLAUDE.md");

  await updateClaudeMcpConfig({
    path: mcpConfigPath,
    force: options.force,
    tokenEnv: options.mcpTokenEnv,
  });
  await updateClaudePermissionSettings(settingsPath, true);
  await writeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
    block: agentGuidanceBlock(),
    force: true,
  });

  return { files: [mcpConfigPath, settingsPath, guidancePath] };
}

async function uninstallClaudeIntegration(): Promise<InstallAgentResult> {
  const configDirectory = resolveClaudeConfigDirectory();
  const mcpConfigPath = resolveClaudeMcpConfigPath();
  const settingsPath = resolve(configDirectory, "settings.json");
  const guidancePath = resolve(configDirectory, "CLAUDE.md");

  await removeClaudeMcpConfig(mcpConfigPath);
  await updateClaudePermissionSettings(settingsPath, false);
  await removeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
  });

  return { files: [mcpConfigPath, settingsPath, guidancePath] };
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

async function installOpenCodeIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const configPath = resolveOpenCodeConfigPath();
  const guidancePath = resolve(dirname(configPath), "AGENTS.md");
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
  await writeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
    block: agentGuidanceBlock({
      rg: "zvec_grep_zvec_grep_rg",
      search: "zvec_grep_zvec_grep_search",
    }),
    force: true,
  });
  return { files: [configPath, guidancePath] };
}

async function uninstallOpenCodeIntegration(): Promise<InstallAgentResult> {
  const configPath = resolveOpenCodeConfigPath();
  const guidancePath = resolve(dirname(configPath), "AGENTS.md");
  await uninstallJsonMcpServer(configPath, "mcp");
  await removeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
  });
  return { files: [configPath, guidancePath] };
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
  const detected = await detectAgentInstallers();
  const targetTokens = [
    ...(parsed.options.installTargets ?? []),
    ...parsed.positionals,
  ];

  if (targetTokens.length > 0) {
    return installersFromTokens(targetTokens, detected);
  }

  if (
    parsed.options.yes === true ||
    !process.stdin.isTTY ||
    !process.stdout.isTTY
  ) {
    return installersFromTokens(["auto"], detected);
  }

  return promptInstallers(action, detected);
}

async function promptInstallers(
  action: "install" | "uninstall",
  detected: ReadonlySet<string>,
): Promise<AgentInstaller[]> {
  console.log(
    action === "install"
      ? "\nChoose agent integrations\n"
      : "\nChoose integrations to remove\n",
  );
  const selected = await promptInstallerSelection(detected);
  return AGENT_INSTALLERS.filter((installer) => selected.has(installer.id));
}

function installersFromTokens(
  tokens: readonly string[],
  detected: ReadonlySet<string> = new Set(),
): AgentInstaller[] {
  const normalized = tokens.flatMap(splitTargetTokens);
  if (normalized.length === 0) {
    return installersFromTokens(["auto"], detected);
  }

  const selected = new Map<string, AgentInstaller>();
  for (const token of normalized) {
    const input = token.toLowerCase();
    const lower = input === "cc" || input === "claude-code" ? "claude" : input;
    if (lower === "none") {
      return [];
    }

    if (lower === "auto") {
      for (const installer of AGENT_INSTALLERS) {
        if (detected.has(installer.id)) {
          selected.set(installer.id, installer);
        }
      }
      continue;
    }

    if (lower === "all") {
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

async function detectAgentInstallers(): Promise<Set<string>> {
  const detected = new Set<string>();
  const checks = await Promise.all(
    AGENT_INSTALLERS.map(async (installer) => ({
      installer,
      available: await executableIsAvailable(installer.executable),
    })),
  );
  for (const check of checks) {
    if (check.available) {
      detected.add(check.installer.id);
    }
  }
  return detected;
}

async function executableIsAvailable(executable: string): Promise<boolean> {
  const path = process.env.PATH ?? "";
  const extensions =
    process.platform === "win32"
      ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM").split(";")
      : [""];
  for (const pathEntry of path.split(delimiter)) {
    const directory = pathEntry || process.cwd();
    for (const extension of extensions) {
      try {
        await access(
          resolve(directory, `${executable}${extension.toLowerCase()}`),
          fileSystemConstants.X_OK,
        );
        return true;
      } catch {
        // Continue searching PATH.
      }
    }
  }
  return false;
}

async function promptInstallerSelection(
  detected: ReadonlySet<string>,
): Promise<Set<string>> {
  if (typeof process.stdin.setRawMode !== "function") {
    return promptInstallerLineSelection(detected);
  }

  let activeIndex = Math.max(
    0,
    AGENT_INSTALLERS.findIndex((installer) => detected.has(installer.id)),
  );
  let renderedLineCount = 0;

  const render = (): void => {
    const lines = installerSelectionLines(activeIndex, detected);

    if (renderedLineCount > 0) {
      process.stdout.write(`\u001b[${renderedLineCount}A`);
    }
    for (const line of lines) {
      process.stdout.write(`\u001b[2K${line}\n`);
    }
    renderedLineCount = lines.length;
  };

  render();
  return new Promise<Set<string>>((resolveSelection) => {
    const wasRaw = process.stdin.isRaw === true;
    const finish = (result: Set<string>): void => {
      process.stdin.removeListener("data", onData);
      process.stdin.setRawMode(wasRaw);
      if (!wasRaw) process.stdin.pause();
      process.stdout.write("\n");
      resolveSelection(result);
    };
    const onData = (chunk: Buffer | string): void => {
      const key = chunk.toString();
      if (key === "\u0003" || key === "\u001b") {
        finish(new Set());
        return;
      }
      if (key === "\u001b[A") {
        activeIndex =
          (activeIndex - 1 + AGENT_INSTALLERS.length) % AGENT_INSTALLERS.length;
        render();
        return;
      }
      if (key === "\u001b[B") {
        activeIndex = (activeIndex + 1) % AGENT_INSTALLERS.length;
        render();
        return;
      }
      if (key.includes("\r") || key.includes("\n")) {
        finish(new Set([AGENT_INSTALLERS[activeIndex]!.id]));
      }
    };

    process.stdin.setRawMode(true);
    process.stdin.resume();
    process.stdin.on("data", onData);
  });
}

export function installerSelectionLines(
  activeIndex: number,
  detected: ReadonlySet<string>,
): string[] {
  const labelWidth = Math.max(
    ...AGENT_INSTALLERS.map((installer) => installer.label.length),
  );
  return [
    ...AGENT_INSTALLERS.map((installer, index) => {
      const marker =
        index === activeIndex ? installSuccess("●") : installDim("○");
      const label = installer.label.padEnd(labelWidth);
      const status = detected.has(installer.id)
        ? installDim("detected")
        : installDim("not found");
      return `  ${marker} ${label}  ${status}`;
    }),
    "",
    installDim("  Use ↑↓ to move · Enter to select"),
  ];
}

async function promptInstallerLineSelection(
  detected: ReadonlySet<string>,
): Promise<Set<string>> {
  AGENT_INSTALLERS.forEach((installer, index) => {
    console.log(
      `  ${index + 1}. ${installer.label} (${detected.has(installer.id) ? "detected" : "not found"})`,
    );
  });
  const defaults = [...detected].join(",") || "none";
  const readline = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  try {
    const answer = await readline.question(`Agents [${defaults}]: `);
    const value = answer.trim() || defaults;
    return new Set(
      installersFromTokens(splitTargetTokens(value), detected).map(
        (installer) => installer.id,
      ),
    );
  } finally {
    readline.close();
  }
}

function printInstallHeader(): void {
  console.log(installAccent("zvec-grep setup"));
  console.log(installDim("─".repeat(40)));
}

function installSuccessMark(): string {
  return installSuccess("✓");
}

function installMutedMark(): string {
  return installDim("○");
}

function installSuccess(value: string): string {
  return installStyle("32", value);
}

function installAccent(value: string): string {
  return installStyle("36", value);
}

function installDim(value: string): string {
  return installStyle("2", value);
}

function installStyle(code: string, value: string): string {
  return process.stdout.isTTY && process.env.NO_COLOR === undefined
    ? `\u001b[${code}m${value}\u001b[0m`
    : value;
}

async function ensureInstalledServer(): Promise<
  Awaited<ReturnType<typeof serverStatus>>
> {
  if (process.env.ZVEC_GREP_INSTALL_SKIP_SERVER === "1") {
    return { running: false, ready: false };
  }
  const { startServer } = await import("../daemon/server-controller.js");
  return startServer({ cliPath: process.argv[1]! });
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
      const progress = createIndexProgressReporter({
        color: parsed.options.color,
      });
      let result: Record<string, unknown>;
      try {
        result = await daemonClient(parsed.options).callTool(
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
          {
            onProgress: (event) => {
              const update = indexProgressFromMessage(event.message);
              if (update) {
                progress.report(update.progress);
              }
            },
          },
        );
      } finally {
        progress.finish();
      }
      console.log(`Workspace index: ${String(result.state ?? "submitted")}`);
      console.log(`Root: ${String(result.root ?? rootPath.absolutePath)}`);
      console.log(`Job: ${String(result.job_id ?? "unknown")}`);
      if (result.state === "failed") {
        throw new Error(serverIndexFailureMessage(result));
      }
    },
    direct: () => runDirectIndex(parsed, rootPath, explicitRoot),
  });
}

function serverIndexFailureMessage(result: Record<string, unknown>): string {
  const error = result.error;
  if (error && typeof error === "object") {
    const code = (error as Record<string, unknown>).code;
    const message = (error as Record<string, unknown>).message;
    if (typeof message === "string" && message.length > 0) {
      if (
        typeof code !== "string" ||
        code.length === 0 ||
        message.includes(`[${code}]`)
      ) {
        return message;
      }
      return `[${code}] ${message}`;
    }
    if (typeof code === "string" && code.length > 0) {
      return `[${code}] Index job failed.`;
    }
  }
  return `Index job ${String(result.job_id ?? "unknown")} failed. Run zg status --mode server for details.`;
}

async function runDirectIndex(
  parsed: ParsedArgs,
  rootPath: RootPath,
  explicitRoot: boolean,
): Promise<void> {
  const zvecGrep = await createZvecGrep(
    createServiceOptions(parsed.options, rootPath.absolutePath),
  );
  const progress = createIndexProgressReporter({ color: parsed.options.color });
  try {
    const infoBefore = await zvecGrep.info({ root: rootPath.absolutePath });
    const schema = resolveAuthorizationSchema(
      parsed.options.embedding ?? process.env.ZVEC_GREP_EMBEDDING,
      infoBefore.collection?.embedding,
    );
    assertEmbeddingModelCompatible(
      infoBefore.collection?.embedding,
      schema,
      parsed.options.rebuild === true,
    );
    const plan = schema
      ? await planRemoteIndexAuthorization({
          info: infoBefore,
          schema,
          rebuild: parsed.options.rebuild,
          serviceOptions: createServiceOptions(
            parsed.options,
            rootPath.absolutePath,
          ),
          store: authorizationStore(parsed.options),
        })
      : undefined;
    const authorizationResolution = plan
      ? await authorizeCliPlan(plan, parsed.options)
      : {};
    printIndexPathFilterTip(parsed.options);
    const result = await withRemoteEmbeddingOperationPermit(
      authorizationResolution.authorization,
      () =>
        zvecGrep.index({
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
        }),
    );
    progress.finish();
    const info = await zvecGrep.info({ root: rootPath.absolutePath });
    printIndexResult(
      "Workspace index",
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
  if (!(await confirmIndexDrop(root, parsed.options.yes === true))) {
    console.log("Index drop cancelled.");
    return;
  }

  const printResult = (removed: boolean): void => {
    console.log(
      removed ? `Dropped index for ${root}` : `No index found for ${root}`,
    );
  };
  await routeByMode({
    mode: resolveClientMode(parsed.options.mode),
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const result = await daemonClient(parsed.options).callTool(
        "zvec_grep_index_drop",
        { root },
      );
      printResult(result.removed === true);
    },
    direct: async () => {
      const zvecGrep = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        printResult(await zvecGrep.dropIndex({ root }));
      } finally {
        await zvecGrep.close();
      }
    },
  });
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
      mcpToolset: parsed.options.mcpToolset,
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
    const status = await serverStatus(parsed.options.home);
    printServerControlStatus(status);
    if (parsed.options.checkReady && !status.ready) {
      throw new Error("zvec-grep server is not ready");
    }
    return;
  }

  const { runDaemonForeground } = await import("../daemon/runtime.js");
  await runDaemonForeground({
    version: readPackageVersion(),
    listen: parsed.options.listen,
    tokenFile: parsed.options.serverTokenFile,
    home: parsed.options.home,
    mcpToolset: parsed.options.mcpToolset,
    serviceOptions: createServiceOptions(parsed.options, process.cwd()),
  });
}

async function runStatus(parsed: ParsedArgs): Promise<void> {
  const root = parsed.positionals[0] ?? process.cwd();
  if (parsed.positionals.length > 1) {
    throw new Error("zg status accepts at most one root path");
  }

  const mode = resolveClientMode(parsed.options.mode);
  const state = await routeByMode({
    mode,
    serverAvailable: () => daemonIsReady(parsed.options.home),
    server: async () => {
      const result = await daemonClient(parsed.options).callTool(
        "zvec_grep_index_status",
        { root: resolve(root) },
      );
      return printServerIndexInfo(
        result as Parameters<typeof printServerIndexInfo>[0],
        parsed.options,
      );
    },
    direct: async () => {
      const zvecGrep = await createZvecGrep(
        createServiceOptions(parsed.options, root),
      );
      try {
        return printAnonymousInfo(
          await zvecGrep.info({ root }),
          parsed.options,
        );
      } finally {
        await zvecGrep.close();
      }
    },
  });
  if (parsed.options.checkReady && state !== "ready") {
    throw new Error(`Workspace index is not ready (state: ${state})`);
  }
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
      const progress = createIndexProgressReporter({
        color: parsed.options.color,
      });
      try {
        const [existing, status] = await Promise.all([
          zvecGrep.collections.info(name),
          zvecGrep.collections.status(name),
        ]);
        const schema = resolveAuthorizationSchema(
          parsed.options.embedding ?? process.env.ZVEC_GREP_EMBEDDING,
          existing?.embedding,
        );
        assertEmbeddingModelCompatible(
          existing?.embedding,
          schema,
          parsed.options.rebuild === true,
        );
        const authorizationCollection = existing
          ? {
              ...existing,
              rootPaths: explicitRoot ? [rootPath] : existing.rootPaths,
            }
          : undefined;
        const infoBefore: ZvecGrepInfoResult = {
          root: rootPath.absolutePath,
          indexed: Boolean(existing?.embedding),
          indexPolicy: existing?.indexPolicy ?? "undecided",
          home: parsed.options.home ?? "",
          indexPath: existing?.path ?? "",
          source: existing?.embedding ? "index" : "unindexed",
          collection: authorizationCollection,
          status,
        };
        const plan = schema
          ? await planRemoteIndexAuthorization({
              info: infoBefore,
              schema,
              rebuild: parsed.options.rebuild,
              serviceOptions: createServiceOptions(parsed.options, undefined),
              store: authorizationStore(parsed.options),
            })
          : undefined;
        const authorization = plan
          ? (await authorizeCliPlan(plan, parsed.options)).authorization
          : undefined;
        const result = await withRemoteEmbeddingOperationPermit(
          authorization,
          () =>
            zvecGrep.collections.index(name, rootPaths, {
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
            }),
        );
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
  if (commandOptions.refresh === "background") {
    console.error(
      "warning: --refresh background requires Server mode; Direct mode uses --refresh off",
    );
  }
  const zvecGrep = await createZvecGrep(
    createServiceOptions(commandOptions, undefined),
  );
  const progress = createIndexProgressReporter({
    color: commandOptions.color,
  });
  try {
    const contextRequest = contextOptions(
      commandOptions,
      queries,
      (progressEvent) => {
        if (progressEvent.phase !== "done") progress.report(progressEvent);
      },
    );
    const info = await directQueryInfo(zvecGrep, commandOptions);
    const plan = commandOptions.rg
      ? undefined
      : await planRemoteSearchAuthorization({
          info,
          search: normalizedDirectSearchInput(
            commandOptions,
            queries,
            info.root,
          ),
          serviceOptions: createServiceOptions(commandOptions, info.root),
          store: authorizationStore(commandOptions),
        });
    const authorizationResolution = plan
      ? await authorizeCliPlan(plan, commandOptions, "local_search")
      : {};
    const effectiveContextRequest =
      authorizationResolution.alternative === "local_search"
        ? ftsFallbackContextRequest(contextRequest)
        : contextRequest;
    const result = await withRemoteEmbeddingOperationPermit(
      authorizationResolution.authorization,
      () => zvecGrep.context(effectiveContextRequest),
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
    if (
      effectiveContextRequest.autoUpdate !== true &&
      indexStatusNeedsRefresh(info.status)
    ) {
      printStaleIndexStatus("idle", indexCompletionFromStatus(info.status));
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
  const response = await daemonClient(options).callTextTool(
    "zvec_grep_search",
    {
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
    },
  );
  console.log(response);
}

function printStaleIndexStatus(
  state: string | undefined,
  completion: { completed?: number; total?: number } | undefined,
): void {
  console.error("status: possibly_stale");
  if (!state) return;
  const progress =
    completion?.completed === undefined || completion.total === undefined
      ? ""
      : ` (${completion.completed}/${completion.total})`;
  console.error(`indexing: ${state}${progress}`);
}

function daemonClient(options: CliOptions): DaemonClient {
  return new DaemonClient({
    serverUrl: resolveServerUrl(),
    home: options.home,
    tokenFile: options.serverTokenFile,
    allowRemote: options.allowRemote,
  });
}

function authorizationStore(
  options: CliOptions,
): RemoteEmbeddingAuthorizationStore {
  return new RemoteEmbeddingAuthorizationStore({
    signingKeyPath: createServiceOptions(options, undefined)
      .authorizationSigningKeyPath,
  });
}

async function authorizeCliPlan(
  plan: RemoteEmbeddingAuthorizationPlan,
  options: CliOptions,
  alternative?: "local_search",
): Promise<{
  authorization?: RemoteEmbeddingOperationPermit;
  alternative?: "local_search";
}> {
  const manager = new RemoteEmbeddingAuthorizationManager(
    authorizationStore(options),
  );
  const existing = await manager.existingWorkspacePermit(plan);
  if (existing) return { authorization: existing };
  if (options.allowRemote) {
    return {
      authorization: await manager.grant(plan, "once"),
    };
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error(
      [
        "Remote Embedding authorization is required.",
        "Re-run with --allow-remote, or grant Workspace authorization:",
        "  zg auth grant --capability embedding --scope workspace",
      ].join("\n"),
    );
  }

  console.error(
    formatRemoteEmbeddingAuthorizationPrompt({
      workspaceRoots: plan.target.workspaceRoots,
      provider: plan.target.provider,
      model: plan.target.model,
      endpoint: plan.target.endpoint,
      data: remoteEmbeddingDisclosureData(plan.disclosure),
    }),
  );
  console.error("");
  console.error("1. Allow once");
  console.error("2. Allow for this workspace");
  if (alternative === "local_search") console.error("3. Use FTS only");
  const cancelChoice = alternative ? 4 : 3;
  console.error(`${cancelChoice}. Cancel`);
  const readline = createInterface({
    input: process.stdin,
    output: process.stderr,
  });
  try {
    const answer = (
      await readline.question(`Choose [1-${cancelChoice}]: `)
    ).trim();
    if (answer === "1") {
      return { authorization: await manager.grant(plan, "once") };
    }
    if (answer === "2") {
      return { authorization: await manager.grant(plan, "workspace") };
    }
    if (answer === "3" && alternative) {
      return { alternative };
    }
    throw new Error(
      "Remote Embedding authorization was declined. No remote data was sent.",
    );
  } finally {
    readline.close();
  }
}

function ftsFallbackContextRequest(
  request: ZvecGrepContextOptions,
): ZvecGrepContextOptions {
  const queries = [
    ...(request.queries ?? []),
    ...(request.query ? [request.query] : []),
    ...(request.routes ?? []).map((route) => route.query),
  ];
  return {
    ...request,
    query: undefined,
    queries: undefined,
    routes: queries.map((query) => ({ mode: "fts", query })),
    autoUpdate: false,
  };
}

function resolveAuthorizationSchema(
  reference: string | undefined,
  existing: CollectionEmbeddingSchema | null | undefined,
): CollectionEmbeddingSchema | undefined {
  if (!reference) return existing ?? undefined;
  const separator = reference.indexOf("/");
  if (separator <= 0 || separator === reference.length - 1) {
    throw new Error(`Invalid embedding reference: ${reference}`);
  }
  const provider = reference.slice(0, separator);
  const model = reference.slice(separator + 1);
  const catalog = getEmbeddingModelCatalogEntry(reference);
  return {
    provider,
    model,
    dimension:
      catalog?.dimension ??
      (existing?.provider === provider && existing.model === model
        ? existing.dimension
        : 1),
    metric:
      catalog?.metric ??
      (existing?.provider === provider && existing.model === model
        ? existing.metric
        : "cosine"),
  };
}

function assertEmbeddingModelCompatible(
  existing: CollectionEmbeddingSchema | null | undefined,
  requested: CollectionEmbeddingSchema | undefined,
  rebuild: boolean,
): void {
  if (!existing || !requested || rebuild) return;
  if (
    existing.provider === requested.provider &&
    existing.model === requested.model
  ) {
    return;
  }
  throw new Error(
    [
      "Embedding model does not match the existing index.",
      `Existing model: ${existing.provider}/${existing.model}`,
      `Requested model: ${requested.provider}/${requested.model}`,
      "Re-run with --rebuild to change the embedding model.",
    ].join("\n"),
  );
}

async function directQueryInfo(
  service: ZvecGrep,
  options: CliOptions,
): Promise<ZvecGrepInfoResult> {
  if (!options.collection) {
    return await service.info({ root: process.cwd() });
  }
  const [collection, status] = await Promise.all([
    service.collections.info(options.collection),
    service.collections.status(options.collection),
  ]);
  if (!collection)
    throw new Error(`Collection not found: ${options.collection}`);
  return {
    root: collection.rootPaths[0]?.absolutePath ?? process.cwd(),
    indexed: collection.embedding !== null,
    indexPolicy: collection.indexPolicy ?? "undecided",
    home: options.home ?? "",
    indexPath: collection.path,
    source: collection.embedding ? "index" : "unindexed",
    collection,
    status,
  };
}

function normalizedDirectSearchInput(
  options: CliOptions,
  queries: readonly string[],
  root: string,
): NormalizedSearchInput {
  const policy = resolveDirectSearchPolicy(options);
  return {
    root,
    queries: !options.rg && queries.length > 0 ? [...queries] : undefined,
    routes: [...(options.routes ?? [])],
    ...policy,
  };
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
  if (status.mcpToolset) console.log(`MCP toolset: ${status.mcpToolset}`);
}

function assertDirectOnlyMode(options: CliOptions, command: string): void {
  if (resolveClientMode(options.mode) === "server") {
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
  const policy = resolveDirectSearchPolicy(options);
  return {
    queries: queries.length > 0 ? queries : undefined,
    rg: options.rg,
    rgOptions: options.rgOptions,
    rgPaths: options.rgPaths,
    routes: options.routes,
    fuse: options.fuse,
    collection: options.collection,
    limit: options.limit,
    autoUpdate: policy.autoUpdate,
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
    authorizationSigningKeyPath: process.env.ZVEC_GREP_AUTHORIZATION_KEY_FILE,
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

function resolveClaudeConfigDirectory(): string {
  return resolve(
    process.env.CLAUDE_CONFIG_DIR ?? resolve(homedir(), ".claude"),
  );
}

function resolveClaudeMcpConfigPath(): string {
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

  const config = parseJsonObject(path, existing);
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

type JsonObject = Record<string, unknown>;

async function updateClaudeMcpConfig(options: {
  path: string;
  force: boolean;
  tokenEnv?: string;
}): Promise<void> {
  const root = await readJsonObject(options.path);
  const currentServers = root.mcpServers;
  if (
    currentServers !== undefined &&
    !isJsonObject(currentServers) &&
    !options.force
  ) {
    throw new Error(
      `Invalid mcpServers configuration in ${options.path}. Re-run with --force to replace it.`,
    );
  }
  const mcpServers = isJsonObject(currentServers) ? currentServers : {};
  const current = mcpServers.zvec_grep;
  if (
    current !== undefined &&
    (!isJsonObject(current) || current.url !== resolveServerUrl()) &&
    !options.force
  ) {
    throw new Error(
      `Existing Claude Code MCP server "zvec_grep" found in ${options.path}. Re-run with --force to replace it.`,
    );
  }

  const existingServer = isJsonObject(current) ? current : {};
  const existingHeaders = isJsonObject(existingServer.headers)
    ? existingServer.headers
    : {};
  mcpServers.zvec_grep = {
    ...existingServer,
    type: "http",
    url: resolveServerUrl(),
    ...(options.tokenEnv
      ? {
          headers: {
            ...existingHeaders,
            Authorization: `Bearer \${${options.tokenEnv}}`,
          },
        }
      : {}),
  };
  root.mcpServers = mcpServers;
  await writeJsonObject(options.path, root);
}

async function removeClaudeMcpConfig(path: string): Promise<void> {
  const source = await readTextFileIfExists(path);
  if (!source.trim()) return;
  const root = parseJsonObject(path, source);
  if (!isJsonObject(root.mcpServers)) return;
  const current = root.mcpServers.zvec_grep;
  if (!isJsonObject(current) || current.url !== resolveServerUrl()) return;

  delete root.mcpServers.zvec_grep;
  if (Object.keys(root.mcpServers).length === 0) {
    delete root.mcpServers;
  }
  await writeJsonObject(path, root);
}

async function updateClaudePermissionSettings(
  path: string,
  grant: boolean,
): Promise<void> {
  const source = await readTextFileIfExists(path);
  if (!source.trim() && !grant) return;
  const root = source.trim() ? parseJsonObject(path, source) : {};
  const currentPermissions = root.permissions;
  if (currentPermissions !== undefined && !isJsonObject(currentPermissions)) {
    throw new Error(`Invalid permissions configuration in ${path}.`);
  }
  const permissions = isJsonObject(currentPermissions)
    ? currentPermissions
    : {};
  const currentAllow = permissions.allow;
  if (currentAllow !== undefined && !Array.isArray(currentAllow)) {
    throw new Error(`Invalid permissions.allow configuration in ${path}.`);
  }
  if (
    Array.isArray(currentAllow) &&
    currentAllow.some((item) => typeof item !== "string")
  ) {
    throw new Error(`Invalid permissions.allow rule in ${path}.`);
  }
  const allow = Array.isArray(currentAllow)
    ? (currentAllow.slice() as string[])
    : [];

  if (grant) {
    if (!allow.includes(CLAUDE_MCP_PERMISSION)) {
      allow.push(CLAUDE_MCP_PERMISSION);
    }
    permissions.allow = allow;
    root.permissions = permissions;
  } else {
    const retained = allow.filter((item) => item !== CLAUDE_MCP_PERMISSION);
    if (retained.length > 0) permissions.allow = retained;
    else delete permissions.allow;
    if (Object.keys(permissions).length > 0) root.permissions = permissions;
    else delete root.permissions;
  }

  await writeJsonObject(path, root);
}

async function readJsonObject(path: string): Promise<JsonObject> {
  const source = await readTextFileIfExists(path);
  return source.trim() ? parseJsonObject(path, source) : {};
}

function parseJsonObject(path: string, source: string): JsonObject {
  let parsed: unknown;
  try {
    parsed = JSON.parse(source);
  } catch (error) {
    throw new Error(`Invalid JSON in ${path}.`, { cause: error });
  }
  if (!isJsonObject(parsed)) {
    throw new Error(`Expected a JSON object in ${path}.`);
  }
  return parsed;
}

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isManagedJsonMcpServer(value: unknown): boolean {
  return isJsonObject(value) && value.url === resolveServerUrl();
}

async function writeJsonObject(path: string, value: JsonObject): Promise<void> {
  await writeTextFileAtomic(path, `${JSON.stringify(value, null, 2)}\n`);
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
default_tools_approval_mode = "approve"
${ZVEC_GREP_CONFIG_END}`;
}

function agentGuidanceBlock(toolNames?: {
  rg: string;
  search: string;
}): string {
  const rgTool = toolNames?.rg ?? "zvec_grep_rg";
  const searchTool = toolNames?.search ?? "zvec_grep_search";
  return `${ZVEC_GREP_AGENTS_START}
## zvec-grep

Route repository search through zvec-grep instead of raw grep, rg, or broad file reads. Choose the tool by intent; indexed search does not need to run before exact search.

- **MCP tools**: Use \`${rgTool}\` first when an exact keyword, text, symbol, filename, path, configuration key, error message, source fragment, literal, or regex anchor is known. A named class, function, or symbol remains an exact anchor even when its file or definition location is unknown. Use \`${searchTool}\` only when the exact anchor is unknown and conceptual discovery is needed.
- **Indexing and status**: Every repository MCP call uses an absolute root visible to the local daemon. Start it with \`zg server on\`. Manage persistent indexes with \`zg index\`, inspect them with \`zg status\`, and inspect the daemon with \`zg server status\`.
- **Remote data authorization**: MCP tool trust does not authorize Remote Embedding. zvec-grep requests its own once or workspace authorization before sending query text or workspace content to a remote provider.
- **Shell fallback**: If the MCP server is unavailable, use \`zg status\`, \`zg query "<query>"\`, and \`zg query --rg "<pattern>"\`.

Prefer focused -g/--glob and -t/--type filters, and exclude dependencies, generated output, caches, build artifacts, and logs unless the task is about those files.
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
