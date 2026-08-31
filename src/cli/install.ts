import {
  applyEdits,
  createScanner,
  findNodeAtLocation,
  modify,
  parse as parseJsonWithComments,
  parseTree,
  SyntaxKind,
  type FormattingOptions,
  type Node as JsoncNode,
  type ParseError,
} from "jsonc-parser";
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
import { parseEnv } from "node:util";
import { REMOTE_EMBEDDING_ELICITATION_UNSUPPORTED_MESSAGE } from "../authorization/prompt.js";
import { resolveServerUrl } from "../client/mode-router.js";
import type { DaemonControlStatus } from "../daemon/server-controller.js";
import type { McpToolset } from "../mcp/toolset.js";
import {
  formatPromptRules,
  ZVEC_GREP_WORKSPACE_EVIDENCE_RULES,
} from "../prompts/zvec-grep-guidance.js";
import type { McpInstallTransport, ParsedArgs } from "./types.js";

type AgentInstaller = {
  id: string;
  aliases?: readonly string[];
  label: string;
  executables: readonly string[];
  detect?: () => Promise<boolean>;
  install: (options: InstallAgentOptions) => Promise<InstallAgentResult>;
  uninstall: () => Promise<InstallAgentResult>;
};

type InstallAgentOptions = {
  force: boolean;
  transport: McpInstallTransport;
  mcpToolset?: McpToolset;
  mcpToolTimeoutSeconds: number;
  mcpTokenEnv?: string;
};

type InstallAgentResult = {
  files: string[];
};

const AGENT_INSTALLERS: readonly AgentInstaller[] = [
  {
    id: "claude",
    aliases: ["cc", "claude-code"],
    label: "Claude Code",
    executables: ["claude"],
    install: installClaudeIntegration,
    uninstall: uninstallClaudeIntegration,
  },
  {
    id: "codex",
    label: "Codex",
    executables: ["codex"],
    install: installCodexIntegration,
    uninstall: uninstallCodexIntegration,
  },
  {
    id: "opencode",
    label: "OpenCode",
    executables: ["opencode"],
    install: installOpenCodeIntegration,
    uninstall: uninstallOpenCodeIntegration,
  },
  {
    id: "cursor",
    label: "Cursor",
    executables: ["cursor"],
    install: installCursorIntegration,
    uninstall: uninstallCursorIntegration,
  },
  {
    id: "qwen",
    aliases: ["qwen-code", "qwencode"],
    label: "Qwen Code",
    executables: ["qwen"],
    install: installQwenIntegration,
    uninstall: uninstallQwenIntegration,
  },
  {
    id: "qoder",
    aliases: ["qodercli", "qoder-cli", "qoderide", "qoder-ide"],
    label: "Qoder",
    executables: ["qoder", "qodercli", "qoder-ide"],
    detect: qoderIdeIsAvailable,
    install: installQoderIntegration,
    uninstall: uninstallQoderIntegration,
  },
];

const ZVEC_GREP_CONFIG_START = "# ZVEC_GREP_START";
const ZVEC_GREP_CONFIG_END = "# ZVEC_GREP_END";
const ZVEC_GREP_AGENTS_START = "<!-- ZVEC_GREP_START -->";
const ZVEC_GREP_AGENTS_END = "<!-- ZVEC_GREP_END -->";
const CLAUDE_MCP_PERMISSION = "mcp__zvec_grep__*";
const NAMESPACED_SEARCH_TOOL = "mcp__zvec_grep__zvec_grep_search";
const NAMESPACED_RG_TOOL = "mcp__zvec_grep__zvec_grep_rg";
const QODER_IDE_MCP_DESCRIPTION = "Managed by zg install";
const DEFAULT_MCP_TOOL_TIMEOUT_SECONDS = 600;

export async function runInstall(parsed: ParsedArgs): Promise<void> {
  printInstallHeader();
  const installers = await resolveInstallers(parsed, "install");
  if (installers.length === 0) {
    console.log("\nNo agent integrations selected.");
    return;
  }
  const transport = await resolveInstallTransport(parsed);

  console.log("\nInstalling integrations\n");
  for (const installer of installers) {
    await installer.install({
      force: parsed.options.force === true,
      transport,
      mcpToolset: parsed.options.mcpToolset,
      mcpToolTimeoutSeconds:
        parsed.options.installMcpToolTimeoutSeconds ??
        DEFAULT_MCP_TOOL_TIMEOUT_SECONDS,
      mcpTokenEnv: parsed.options.installMcpTokenEnv,
    });
    console.log(`  ${installSuccessMark()} ${installer.label}`);
    console.log("    MCP       configured");
    console.log("");
  }

  const server = await ensureInstalledServer(parsed.options.mcpToolset);
  if (server.ready) {
    console.log(`  ${installSuccessMark()} Server`);
    console.log(`    ready at ${server.serverUrl ?? resolveServerUrl()}`);
  } else {
    console.log(`  ${installMutedMark()} Server`);
    console.log("    not started; run `zg server on`");
  }
  if (transport === "stdio") {
    console.log(`  ${installSuccessMark()} Connection`);
    console.log("    stdio; reconnects start the server automatically");
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

export async function runUninstall(parsed: ParsedArgs): Promise<void> {
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
    block: codexConfigBlock(options),
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
    transport: options.transport,
    mcpToolset: options.mcpToolset,
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
    server:
      options.transport === "stdio"
        ? {
            type: "local",
            command: stdioCommand(options.mcpToolset),
            enabled: true,
            timeout: options.mcpToolTimeoutSeconds * 1_000,
          }
        : {
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
      search: "zvec_grep_zvec_grep_search",
      rg: "zvec_grep_zvec_grep_rg",
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
    server:
      options.transport === "stdio"
        ? { command: "zg", args: stdioArgs(options.mcpToolset) }
        : {
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

async function installQwenIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const qwenHome = await resolveQwenHome();
  const settingsPath = resolve(qwenHome, "settings.json");
  const guidancePath = resolve(qwenHome, "QWEN.md");

  const warnings = await updateQwenSettings({
    ...options,
    path: settingsPath,
  });
  await writeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
    block: agentGuidanceBlock({
      search: NAMESPACED_SEARCH_TOOL,
      rg: NAMESPACED_RG_TOOL,
    }),
    force: true,
  });

  for (const warning of warnings) {
    console.warn(`    warning   ${warning}`);
  }

  return { files: [settingsPath, guidancePath] };
}

async function uninstallQwenIntegration(): Promise<InstallAgentResult> {
  const qwenHome = await resolveQwenHome();
  const settingsPath = resolve(qwenHome, "settings.json");
  const guidancePath = resolve(qwenHome, "QWEN.md");

  await removeQwenSettings(settingsPath);
  await removeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
  });

  return { files: [settingsPath, guidancePath] };
}

async function installQoderIntegration(
  options: InstallAgentOptions,
): Promise<InstallAgentResult> {
  const qoderHome = resolveQoderHome();
  const settingsPath = resolve(qoderHome, "settings.json");
  const ideMcpPath = await resolveQoderIdeMcpPath();
  const guidancePath = resolve(qoderHome, "AGENTS.md");

  await assertQoderMcpSettingsReplaceable(
    settingsPath,
    "Qoder CLI",
    options.force,
    isManagedJsonMcpServer,
  );
  await assertQoderMcpSettingsReplaceable(
    ideMcpPath,
    "Qoder IDE",
    options.force,
    isManagedQoderIdeMcpServer,
  );

  const warnings = await updateQoderSettings({
    ...options,
    path: settingsPath,
  });
  await updateQoderIdeSettings({ ...options, path: ideMcpPath });
  await writeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
    block: agentGuidanceBlock({
      search: NAMESPACED_SEARCH_TOOL,
      rg: NAMESPACED_RG_TOOL,
      qoderAuthorizationRecovery: true,
    }),
    force: true,
  });

  for (const warning of warnings) {
    console.warn(`    warning   ${warning}`);
  }

  return { files: [settingsPath, ideMcpPath, guidancePath] };
}

async function uninstallQoderIntegration(): Promise<InstallAgentResult> {
  const qoderHome = resolveQoderHome();
  const settingsPath = resolve(qoderHome, "settings.json");
  const ideMcpPaths = qoderIdeMcpPathsForUninstall();
  const guidancePath = resolve(qoderHome, "AGENTS.md");

  await removeQoderSettings(settingsPath);
  for (const path of ideMcpPaths) {
    await removeQoderIdeSettings(path);
  }
  await removeMarkedFile({
    path: guidancePath,
    startMarker: ZVEC_GREP_AGENTS_START,
    endMarker: ZVEC_GREP_AGENTS_END,
  });

  return { files: [settingsPath, ...ideMcpPaths, guidancePath] };
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

async function resolveInstallTransport(
  parsed: ParsedArgs,
): Promise<McpInstallTransport> {
  if (parsed.options.installMcpTransport) {
    return parsed.options.installMcpTransport;
  }
  if (
    parsed.options.yes === true ||
    !process.stdin.isTTY ||
    !process.stdout.isTTY
  ) {
    return "stdio";
  }

  const readline = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  try {
    const answer = await readline.question(
      "MCP transport [stdio] (stdio/http): ",
    );
    const value = answer.trim().toLowerCase();
    if (!value || value === "stdio") return "stdio";
    if (value === "http") return "http";
    throw new Error("MCP transport must be stdio or http");
  } finally {
    readline.close();
  }
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
    const lower = token.toLowerCase();
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
        candidate.id === lower ||
        candidate.aliases?.includes(lower) ||
        candidate.label.toLowerCase() === lower,
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
    AGENT_INSTALLERS.map(async (installer) => {
      const executableDetected = (
        await Promise.all(
          installer.executables.map((executable) =>
            executableIsAvailable(executable),
          ),
        )
      ).some(Boolean);
      return {
        installer,
        available: executableDetected || (await installer.detect?.()) === true,
      };
    }),
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

async function ensureInstalledServer(
  mcpToolset?: McpToolset,
): Promise<DaemonControlStatus> {
  if (process.env.ZVEC_GREP_INSTALL_SKIP_SERVER === "1") {
    return { running: false, ready: false };
  }
  const { startServer } = await import("../daemon/server-controller.js");
  return startServer({ cliPath: process.argv[1]!, mcpToolset });
}

function splitTargetTokens(value: string): string[] {
  return value
    .split(/[,\s]+/)
    .map((target) => target.trim())
    .filter((target) => target.length > 0);
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

function resolveQoderHome(): string {
  return resolve(process.env.QODER_CONFIG_DIR || resolve(homedir(), ".qoder"));
}

async function resolveQoderIdeMcpPath(): Promise<string> {
  const configured = process.env.QODER_IDE_MCP_PATH?.trim();
  if (configured) return resolve(configured);

  const candidates = qoderIdeMcpPathCandidates();
  for (const candidate of candidates) {
    if (await fileExists(candidate)) return candidate;
  }
  return candidates[0]!;
}

function qoderIdeMcpPathsForUninstall(): string[] {
  const configured = process.env.QODER_IDE_MCP_PATH?.trim();
  return configured ? [resolve(configured)] : qoderIdeMcpPathCandidates();
}

function qoderIdeMcpPathCandidates(): string[] {
  const qoderHome = resolve(homedir(), ".qoder");
  const sharedClientCache =
    process.platform === "darwin"
      ? resolve(
          homedir(),
          "Library",
          "Application Support",
          "Qoder",
          "SharedClientCache",
        )
      : process.platform === "win32"
        ? resolve(
            process.env.APPDATA || resolve(homedir(), "AppData", "Roaming"),
            "Qoder",
            "SharedClientCache",
          )
        : resolve(
            process.env.XDG_CONFIG_HOME || resolve(homedir(), ".config"),
            "Qoder",
            "SharedClientCache",
          );
  return [
    resolve(sharedClientCache, "mcp.json"),
    resolve(sharedClientCache, "extension", "local", "mcp.json"),
    resolve(qoderHome, "shared_client", "mcp.json"),
    resolve(qoderHome, "shared_client", "extension", "local", "mcp.json"),
  ];
}

async function qoderIdeIsAvailable(): Promise<boolean> {
  const configured = process.env.QODER_IDE_EXECUTABLE?.trim();
  const candidates = configured
    ? [resolve(configured)]
    : qoderIdeExecutableCandidates();
  for (const candidate of candidates) {
    try {
      await access(candidate, fileSystemConstants.X_OK);
      return true;
    } catch {
      // Try the next platform-specific installation path.
    }
  }
  return false;
}

function qoderIdeExecutableCandidates(): string[] {
  if (process.platform === "darwin") {
    return [
      resolve(
        homedir(),
        "Applications",
        "Qoder IDE.app",
        "Contents",
        "MacOS",
        "Qoder",
      ),
      resolve("/Applications", "Qoder IDE.app", "Contents", "MacOS", "Qoder"),
      resolve(
        homedir(),
        "Applications",
        "Qoder.app",
        "Contents",
        "MacOS",
        "Qoder",
      ),
      resolve("/Applications", "Qoder.app", "Contents", "MacOS", "Qoder"),
    ];
  }
  if (process.platform === "win32") {
    const localPrograms = resolve(
      process.env.LOCALAPPDATA || resolve(homedir(), "AppData", "Local"),
      "Programs",
    );
    return [
      resolve(localPrograms, "Qoder IDE", "Qoder IDE.exe"),
      resolve(localPrograms, "Qoder", "Qoder.exe"),
      ...(process.env.ProgramFiles
        ? [
            resolve(process.env.ProgramFiles, "Qoder IDE", "Qoder IDE.exe"),
            resolve(process.env.ProgramFiles, "Qoder", "Qoder.exe"),
          ]
        : []),
    ];
  }
  return [
    "/usr/share/qoder-ide/qoder-ide",
    "/usr/share/qoder-ide/bin/qoder-ide",
    "/usr/bin/qoder-ide",
    "/usr/share/qoder/bin/qoder",
  ];
}

async function fileExists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") return false;
    throw error;
  }
}

async function resolveQwenHome(): Promise<string> {
  const defaultQwenHome = resolve(homedir(), ".qwen");
  const configured = process.env.QWEN_HOME;
  if (configured) {
    return resolveQwenHomeValue(configured);
  }

  // Qwen Code treats an explicitly exported empty QWEN_HOME as the default
  // directory and does not replace it from a user .env file.
  if (Object.hasOwn(process.env, "QWEN_HOME")) {
    return defaultQwenHome;
  }

  const discovered =
    (await readQwenHomeFromEnv(resolve(defaultQwenHome, ".env"))) ??
    (await readQwenHomeFromEnv(resolve(homedir(), ".env")));
  return discovered ? resolveQwenHomeValue(discovered) : defaultQwenHome;
}

async function readQwenHomeFromEnv(path: string): Promise<string | undefined> {
  const source = await readTextFileIfExists(path);
  if (!source) return undefined;

  try {
    return parseEnv(source).QWEN_HOME || undefined;
  } catch {
    // Match Qwen Code's quiet dotenv bootstrap behavior.
    return undefined;
  }
}

function resolveQwenHomeValue(value: string): string {
  if (value === "~") return homedir();
  if (value.startsWith("~/") || value.startsWith("~\\")) {
    return resolve(
      homedir(),
      ...value
        .slice(2)
        .split(/[/\\]+/)
        .filter(Boolean),
    );
  }
  return resolve(value);
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
  transport: McpInstallTransport;
  mcpToolset?: McpToolset;
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
    !isManagedJsonMcpServer(current) &&
    !options.force
  ) {
    throw new Error(
      `Existing Claude Code MCP server "zvec_grep" found in ${options.path}. Re-run with --force to replace it.`,
    );
  }

  const existingServer = isJsonObject(current) ? current : {};
  const {
    type: _type,
    url: _url,
    command: _command,
    args: _args,
    headers: _headers,
    ...retained
  } = existingServer;
  mcpServers.zvec_grep =
    options.transport === "stdio"
      ? {
          ...retained,
          type: "stdio",
          command: "zg",
          args: stdioArgs(options.mcpToolset),
        }
      : {
          ...retained,
          type: "http",
          url: resolveServerUrl(),
          ...(options.tokenEnv
            ? {
                headers: {
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
  if (!isManagedJsonMcpServer(current)) return;

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

async function updateQwenSettings(
  options: InstallAgentOptions & { path: string },
): Promise<string[]> {
  const root = await updateJsoncMcpSettings({
    path: options.path,
    force: options.force,
    label: "Qwen Code",
    server: qwenMcpServer(options),
    isManaged: isManagedQwenMcpServer,
  });
  return contextFileWarnings(root, "QWEN.md");
}

async function removeQwenSettings(path: string): Promise<void> {
  await removeJsoncMcpSettings(path, "Qwen Code", isManagedQwenMcpServer);
}

async function updateQoderSettings(
  options: InstallAgentOptions & { path: string },
): Promise<string[]> {
  const root = await updateJsoncMcpSettings({
    path: options.path,
    force: options.force,
    label: "Qoder",
    server: qoderMcpServer(options),
    isManaged: isManagedJsonMcpServer,
  });
  return contextFileWarnings(root, "AGENTS.md");
}

async function removeQoderSettings(path: string): Promise<void> {
  await removeJsoncMcpSettings(path, "Qoder", isManagedJsonMcpServer);
}

async function updateQoderIdeSettings(
  options: InstallAgentOptions & { path: string },
): Promise<void> {
  await updateJsoncMcpSettings({
    path: options.path,
    force: options.force,
    label: "Qoder IDE",
    server: qoderIdeMcpServer(options),
    isManaged: isManagedQoderIdeMcpServer,
  });
}

async function removeQoderIdeSettings(path: string): Promise<void> {
  await removeJsoncMcpSettings(path, "Qoder IDE", isManagedQoderIdeMcpServer);
}

function qwenMcpServer(options: InstallAgentOptions): Record<string, unknown> {
  const timeout = options.mcpToolTimeoutSeconds * 1_000;
  // Qwen otherwise defers MCP schemas behind tool_search, so the managed
  // search tool would not be directly callable on the first turn.
  if (options.transport === "stdio") {
    return {
      command: "zg",
      args: stdioArgs(options.mcpToolset),
      timeout,
      alwaysLoadTools: true,
      trust: true,
    };
  }

  return {
    httpUrl: resolveServerUrl(),
    timeout,
    alwaysLoadTools: true,
    trust: true,
    ...(options.mcpTokenEnv
      ? {
          headers: {
            Authorization: `Bearer \${${options.mcpTokenEnv}}`,
          },
        }
      : {}),
  };
}

function qoderMcpServer(options: InstallAgentOptions): Record<string, unknown> {
  const timeout = options.mcpToolTimeoutSeconds * 1_000;
  if (options.transport === "stdio") {
    return {
      command: "zg",
      args: stdioArgs(options.mcpToolset),
      timeout,
      trust: true,
    };
  }

  return {
    type: "http",
    url: resolveServerUrl(),
    timeout,
    trust: true,
    ...(options.mcpTokenEnv
      ? {
          headers: {
            Authorization: `Bearer \${${options.mcpTokenEnv}}`,
          },
        }
      : {}),
  };
}

function qoderIdeMcpServer(
  options: InstallAgentOptions,
): Record<string, unknown> {
  const timeout = options.mcpToolTimeoutSeconds * 1_000;
  if (options.transport === "stdio") {
    const launch = stableQoderIdeStdioLaunch(options.mcpToolset);
    return {
      command: launch.command,
      args: launch.args,
      timeout,
      description: QODER_IDE_MCP_DESCRIPTION,
    };
  }

  return {
    type: "sse",
    url: resolveServerUrl(),
    timeout,
    description: QODER_IDE_MCP_DESCRIPTION,
    ...(options.mcpTokenEnv
      ? {
          headers: {
            Authorization: `Bearer \${${options.mcpTokenEnv}}`,
          },
        }
      : {}),
  };
}

type JsoncMcpSettingsOptions = {
  path: string;
  force: boolean;
  label: string;
  server: Record<string, unknown>;
  isManaged: (value: unknown) => boolean;
};

async function assertQoderMcpSettingsReplaceable(
  path: string,
  label: "Qoder CLI" | "Qoder IDE",
  force: boolean,
  isManaged: (value: unknown) => boolean,
): Promise<void> {
  const existing = await readTextFileIfExists(path);
  const source = existing.trim() ? existing : "{}\n";
  const root = parseJsoncSettings(path, source, label);
  validateMcpSettingsContainer(path, root);
  const mcpServers = isJsonObject(root.mcpServers) ? root.mcpServers : {};
  if (
    mcpServers.zvec_grep !== undefined &&
    !isManaged(mcpServers.zvec_grep) &&
    !force
  ) {
    throw new Error(
      `Existing unmanaged zvec_grep MCP server found in ${path}. Re-run with --force to replace it for ${label}.`,
    );
  }
}

async function updateJsoncMcpSettings(
  options: JsoncMcpSettingsOptions,
): Promise<JsonObject> {
  const existing = await readTextFileIfExists(options.path);
  let source = existing.trim() ? existing : "{}\n";
  const root = parseJsoncSettings(options.path, source, options.label);
  validateMcpSettingsContainer(options.path, root);

  const mcpServers = isJsonObject(root.mcpServers) ? root.mcpServers : {};
  const current = mcpServers.zvec_grep;
  if (current !== undefined && !options.isManaged(current) && !options.force) {
    throw new Error(
      `Existing unmanaged zvec_grep MCP server found in ${options.path}. Re-run with --force to replace it for ${options.label}.`,
    );
  }

  source = editJsonWithComments(
    source,
    ["mcpServers", "zvec_grep"],
    options.server,
  );
  await writeTextFileAtomic(options.path, ensureTrailingNewline(source));
  return root;
}

async function removeJsoncMcpSettings(
  path: string,
  label: string,
  isManaged: (value: unknown) => boolean,
): Promise<void> {
  const existing = await readTextFileIfExists(path);
  if (!existing.trim()) return;

  let source = existing;
  const root = parseJsoncSettings(path, source, label);
  validateMcpSettingsContainer(path, root);
  const mcpServers = isJsonObject(root.mcpServers) ? root.mcpServers : {};

  if (isManaged(mcpServers.zvec_grep)) {
    source = hasJsoncComments(source)
      ? removeJsoncPropertyPreservingComments(source, [
          "mcpServers",
          "zvec_grep",
        ])
      : editJsonWithComments(
          source,
          Object.keys(mcpServers).length === 1
            ? ["mcpServers"]
            : ["mcpServers", "zvec_grep"],
          undefined,
        );
  }
  if (source !== existing) {
    await writeTextFileAtomic(path, ensureTrailingNewline(source));
  }
}

function parseJsoncSettings(
  path: string,
  source: string,
  label: string,
): JsonObject {
  const errors: ParseError[] = [];
  const parsed = parseJsonWithComments(source, errors, {
    allowTrailingComma: false,
    disallowComments: false,
  });
  if (errors.length > 0 || !isJsonObject(parsed)) {
    throw new Error(`Invalid ${label} configuration in ${path}.`);
  }
  return parsed;
}

function validateMcpSettingsContainer(path: string, root: JsonObject): void {
  if (root.mcpServers !== undefined && !isJsonObject(root.mcpServers)) {
    throw new Error(`Invalid mcpServers configuration in ${path}.`);
  }
}

function editJsonWithComments(
  source: string,
  path: readonly (string | number)[],
  value: unknown,
): string {
  return applyEdits(
    source,
    modify(source, [...path], value, {
      formattingOptions: jsonFormattingOptions(source),
    }),
  );
}

function hasJsoncComments(source: string): boolean {
  const scanner = createScanner(source, false);
  for (
    let token = scanner.scan();
    token !== SyntaxKind.EOF;
    token = scanner.scan()
  ) {
    if (
      token === SyntaxKind.LineCommentTrivia ||
      token === SyntaxKind.BlockCommentTrivia
    ) {
      return true;
    }
  }
  return false;
}

function removeJsoncPropertyPreservingComments(
  source: string,
  path: readonly (string | number)[],
): string {
  const root = parseTree(source);
  if (!root) return source;

  const valueNode = findNodeAtLocation(root, [...path]);
  const propertyNode = valueNode?.parent;
  const objectNode = propertyNode?.parent;
  if (
    propertyNode?.type !== "property" ||
    objectNode?.type !== "object" ||
    !objectNode.children
  ) {
    return source;
  }

  const propertyIndex = objectNode.children.indexOf(propertyNode);
  if (propertyIndex < 0) return source;

  const ranges = [nodeRange(propertyNode)];
  const previousProperty = objectNode.children[propertyIndex - 1];
  const nextProperty = objectNode.children[propertyIndex + 1];
  const separatorOffset = nextProperty
    ? findComma(
        source,
        propertyNode.offset + propertyNode.length,
        nextProperty.offset,
      )
    : previousProperty
      ? findComma(
          source,
          previousProperty.offset + previousProperty.length,
          propertyNode.offset,
        )
      : undefined;
  if (separatorOffset !== undefined) {
    ranges.push({ offset: separatorOffset, length: 1 });
  }

  return ranges
    .sort((left, right) => right.offset - left.offset)
    .reduce(
      (current, range) =>
        current.slice(0, range.offset) +
        current.slice(range.offset + range.length),
      source,
    );
}

function nodeRange(node: JsoncNode): { offset: number; length: number } {
  return { offset: node.offset, length: node.length };
}

function findComma(
  source: string,
  startOffset: number,
  endOffset: number,
): number | undefined {
  const scanner = createScanner(source, false);
  scanner.setPosition(startOffset);
  for (
    let token = scanner.scan();
    token !== SyntaxKind.EOF;
    token = scanner.scan()
  ) {
    const tokenOffset = scanner.getTokenOffset();
    if (tokenOffset >= endOffset) return undefined;
    if (token === SyntaxKind.CommaToken) return tokenOffset;
  }
  return undefined;
}

function jsonFormattingOptions(source: string): FormattingOptions {
  const eol = source.includes("\r\n") ? "\r\n" : "\n";
  const indentation = source.match(/^[ \t]+(?=")/m)?.[0];
  if (indentation?.startsWith("\t")) {
    return { insertSpaces: false, tabSize: 1, eol };
  }
  return {
    insertSpaces: true,
    tabSize: indentation?.length ?? 2,
    eol,
  };
}

function contextFileWarnings(root: JsonObject, fileName: string): string[] {
  if (!isJsonObject(root.context)) return [];
  const configured = root.context.fileName;
  const fileNames =
    typeof configured === "string"
      ? [configured]
      : Array.isArray(configured) &&
          configured.every((value) => typeof value === "string")
        ? (configured as string[])
        : undefined;
  if (!fileNames || fileNames.includes(fileName)) return [];
  return [
    `context.fileName does not include ${fileName}; the installed zvec-grep guidance may not be loaded.`,
  ];
}

function isManagedQwenMcpServer(value: unknown): boolean {
  return (
    isJsonObject(value) &&
    (value.httpUrl === resolveServerUrl() ||
      (value.command === "zg" && isStdioArgs(value.args)))
  );
}

function ensureTrailingNewline(source: string): string {
  return source.endsWith("\n") ? source : `${source}\n`;
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
  if (!isJsonObject(value)) return false;
  if (value.url === resolveServerUrl()) return true;
  if (value.command === "zg" && isStdioArgs(value.args)) return true;
  return (
    value.type === "local" &&
    Array.isArray(value.command) &&
    (value.command.length === 3 || value.command.length === 5) &&
    value.command[0] === "zg" &&
    value.command[1] === "server" &&
    value.command[2] === "--stdio" &&
    (value.command.length === 3 ||
      (value.command[3] === "--mcp-toolset" &&
        (value.command[4] === "agent" || value.command[4] === "full")))
  );
}

function isManagedQoderIdeMcpServer(value: unknown): boolean {
  return (
    isJsonObject(value) &&
    value.description === QODER_IDE_MCP_DESCRIPTION &&
    ((typeof value.command === "string" && isStableStdioArgs(value.args)) ||
      (value.type === "sse" && typeof value.url === "string"))
  );
}

function isStdioArgs(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    (value.length === 2 || value.length === 4) &&
    value[0] === "server" &&
    value[1] === "--stdio" &&
    (value.length === 2 ||
      (value[2] === "--mcp-toolset" &&
        (value[3] === "agent" || value[3] === "full")))
  );
}

function isStableStdioArgs(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    (value.length === 3 || value.length === 5) &&
    typeof value[0] === "string" &&
    value[1] === "server" &&
    value[2] === "--stdio" &&
    (value.length === 3 ||
      (value[3] === "--mcp-toolset" &&
        (value[4] === "agent" || value[4] === "full")))
  );
}

function stdioArgs(mcpToolset?: McpToolset): string[] {
  return [
    "server",
    "--stdio",
    ...(mcpToolset ? ["--mcp-toolset", mcpToolset] : []),
  ];
}

function stdioCommand(mcpToolset?: McpToolset): string[] {
  return ["zg", ...stdioArgs(mcpToolset)];
}

function stableQoderIdeStdioLaunch(mcpToolset?: McpToolset): {
  command: string;
  args: string[];
} {
  const cliPath = process.argv[1];
  if (!cliPath) {
    return { command: "zg", args: stdioArgs(mcpToolset) };
  }
  return {
    command: process.execPath,
    args: [resolve(cliPath), ...stdioArgs(mcpToolset)],
  };
}

function tomlStringArray(values: readonly string[]): string {
  return `[${values.map((value) => JSON.stringify(value)).join(", ")}]`;
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

function codexConfigBlock(options: InstallAgentOptions): string {
  return `${ZVEC_GREP_CONFIG_START}
[mcp_servers.zvec_grep]
${
  options.transport === "stdio"
    ? `command = "zg"
args = ${tomlStringArray(stdioArgs(options.mcpToolset))}`
    : `url = "${resolveServerUrl()}"`
}
${
  options.mcpTokenEnv
    ? `bearer_token_env_var = "${options.mcpTokenEnv}"
`
    : ""
}tool_timeout_sec = ${options.mcpToolTimeoutSeconds}
default_tools_approval_mode = "approve"
${ZVEC_GREP_CONFIG_END}`;
}

function agentGuidanceBlock(toolNames?: {
  search: string;
  rg: string;
  qoderAuthorizationRecovery?: boolean;
}): string {
  const searchTool = toolNames?.search ?? "zvec_grep_search";
  const rgTool = toolNames?.rg ?? "zvec_grep_rg";
  const exactLookupRoute = `\`${rgTool}\` when it is listed by the current host; otherwise native Grep or \`rg\``;
  const qoderAuthorizationRecovery = toolNames?.qoderAuthorizationRecovery
    ? `

${formatPromptRules("### Qoder Remote Embedding authorization recovery", [
  `When \`${searchTool}\` needs \`remote_embedding_authorization\` and the current Qoder host returns \`code = 51500 message = method not found: No request handler configured\` without showing an authorization form, returns \`${REMOTE_EMBEDDING_ELICITATION_UNSUPPORTED_MESSAGE}\`, or reports that authorization was declined or cancelled without showing the user an authorization form, treat it as a Qoder client interaction limitation: the host lacks the server-to-client \`elicitation/create\` request handler. The outer MCP \`tools/call\` has already reached the registered MCP server and tool, so do not diagnose this error as a disconnected or missing MCP server or as a missing tool. Apply this recovery only to the Remote Embedding authorization path, not to arbitrary 51500 failures. Do not immediately fall back to broad file reads, do not treat it as a missing API credential, and do not grant access silently. If the user actually declined a displayed authorization form, respect that decision and do not ask again.`,
  "Use the current Qoder host's built-in user-question tool (`ask_user_question` in Qoder IDE or `AskUserQuestion` in Qoder CLI/SDK) to offer exactly these choices: allow Remote Embedding for this workspace, use local FTS only, or cancel. Explain that workspace approval may send query text and selected workspace content to the configured provider and endpoint and may incur provider charges.",
  'Only after the user explicitly chooses workspace approval, run `zg auth grant "<absolute-root>" --capability embedding --scope workspace`, substituting the same absolute root used by the failed search, and then retry the original search call once. Do not use `--allow-remote`; it applies only to one CLI command and does not authorize the MCP retry.',
  `If the user chooses local FTS, retry \`${searchTool}\` once with the original search text in \`fts\`, omit \`query\`, \`queries\`, and \`vector\`, set \`autoUpdate\` to \`false\` and \`freshness\` to \`eventual\`, and preserve \`root\`, filters, and limits. This route is lexical-only, does not refresh the remote-embedding index, and sends no query text or workspace content to a remote Embedding provider.`,
  "If the user cancels, the grant command fails, or interactive user input is unavailable, stop and report that no remote data was sent. Provider credentials and Remote Embedding data authorization are separate; never request or modify an API key merely to resolve this interaction error.",
])}`
    : "";
  return `${ZVEC_GREP_AGENTS_START}
## zvec-grep

Choose the evidence source before the retrieval mode.

${formatPromptRules(
  "### Workspace evidence",
  ZVEC_GREP_WORKSPACE_EVIDENCE_RULES,
)}

${formatPromptRules("### Retrieval routing", [
  `When an exact word, phrase, name, date, identifier, filename, path, configuration key, error message, source fragment, literal, or regex is known and locating its occurrences is sufficient, use ${exactLookupRoute}.`,
  `Use \`${searchTool}\` when wording or location is unknown, or when the answer requires semantic, conceptual, fuzzy, or paraphrase discovery; relationships, chronology, causality, architecture, or data or control flow; or comparison or synthesis across files, sections, or documents.`,
  `For a mixed task with exact anchors that still requires relationships or cross-file synthesis, call \`${searchTool}\` with the concept and anchors, then use ${exactLookupRoute} for focused follow-up.`,
  `When no sufficient exact anchor is available and the user asks whether conceptually related material exists locally, make at most one focused \`${searchTool}\` probe using the question plus distinctive names, dates, or terms. This probe does not apply to exact quotations, configuration keys, filenames, regexes, or exhaustive occurrence requests. Continue only when results are relevant; otherwise stop and report that the indexed workspace did not establish the answer.`,
  "Before broad file reads or delegating workspace discovery, use the appropriate search route. Do not delegate solely to locate material, and stop when the evidence is sufficient.",
])}

${formatPromptRules("### Search evidence", [
  "Search results include bounded source snippets. Treat a sufficient snippet as already-read evidence, and read a cited file only when a required detail falls outside the snippet.",
])}

${formatPromptRules("### Freshness and index lifecycle", [
  "Pass a daemon-visible absolute `root` on every zvec-grep workspace call.",
  "Read `freshness` and `background_refresh` from search results without a status preflight.",
  "When results are `served_from_current_index`, use them when sufficient instead of waiting for the background refresh.",
  `If the index is missing but exact or regex lookup can answer the task, use ${exactLookupRoute}.`,
  "Creating, rebuilding, or dropping a persistent index requires an explicit user request or authorization; never do so silently.",
])}
${qoderAuthorizationRecovery}
${ZVEC_GREP_AGENTS_END}`;
}
