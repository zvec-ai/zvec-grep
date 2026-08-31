import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { createServer } from "node:net";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { parse as parseJsonWithComments } from "jsonc-parser";
import { installerSelectionLines } from "../dist/cli/install.js";
import { ZVEC_GREP_WORKSPACE_EVIDENCE_RULES } from "../dist/prompts/zvec-grep-guidance.js";

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");

test("interactive installer marker follows the active agent", () => {
  const detected = new Set(["claude", "codex"]);
  const claude = installerSelectionLines(0, detected);
  const codex = installerSelectionLines(1, detected);
  const qwen = installerSelectionLines(4, detected);

  assert.match(claude[0], /● Claude Code\s+detected/);
  assert.match(claude[1], /○ Codex\s+detected/);
  assert.match(codex[0], /○ Claude Code\s+detected/);
  assert.match(codex[1], /● Codex\s+detected/);
  assert.match(qwen[0], /○ Claude Code\s+detected/);
  assert.match(qwen[1], /○ Codex\s+detected/);
  assert.match(qwen[2], /○ OpenCode\s+not found/);
  assert.match(qwen[3], /○ Cursor\s+not found/);
  assert.match(qwen[4], /● Qwen Code\s+not found/);
  assert.match(codex.at(-1), /Use ↑↓ to move · Enter to select/);
  assert.doesNotMatch(codex.join("\n"), /Space|\[●\]/);
});

test("install starts the shared server with the selected MCP toolset", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-server-"),
  );
  const home = join(temporaryDirectory, "home");
  const port = await availablePort();
  const serverUrl = `http://127.0.0.1:${port}/mcp`;
  await mkdir(join(temporaryDirectory, ".zvec-grep"), { recursive: true });
  await writeFile(
    join(temporaryDirectory, ".zvec-grep", "config.json"),
    `${JSON.stringify({
      version: 1,
      client: { serverUrl },
      server: { host: "127.0.0.1", port },
    })}\n`,
  );
  const environment = {
    ...process.env,
    HOME: temporaryDirectory,
    USERPROFILE: temporaryDirectory,
    CODEX_HOME: join(temporaryDirectory, ".codex"),
    ZVEC_GREP_HOME: home,
  };
  t.after(async () => {
    await execFileAsync(
      process.execPath,
      [cliPath, "server", "off", "--home", home],
      { env: environment },
    ).catch(() => undefined);
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const { stdout } = await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "codex", "--mcp-toolset", "full", "--yes"],
    { env: environment },
  );
  assert.match(stdout, new RegExp(`ready at ${serverUrl}`));
  const { stdout: statusOutput } = await execFileAsync(
    process.execPath,
    [cliPath, "server", "status", "--check-ready", "--home", home],
    { env: environment },
  );
  assert.match(statusOutput, /MCP toolset: full/);
  const config = await readFile(
    join(temporaryDirectory, ".codex", "config.toml"),
    "utf8",
  );
  assertCodexStableStdioLaunch(config, ["--mcp-toolset", "full"]);
});

test("Codex installer removes orphaned managed markers", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(codexHome, { recursive: true });
  await writeFile(
    configPath,
    ["[mcp_servers.other]", 'command = "other"', "# ZVEC_GREP_END", ""].join(
      "\n",
    ),
  );

  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "codex", "--yes"],
    {
      env: {
        ...process.env,
        CODEX_HOME: codexHome,
        ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
      },
    },
  );

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]/);
  assertCodexStableStdioLaunch(installed);
  assert.doesNotMatch(installed, /^bearer_token_env_var\s*=/m);
  assert.doesNotMatch(installed, /^url\s*=/m);
  assert.match(installed, /^tool_timeout_sec = 600$/m);
  assert.match(installed, /^default_tools_approval_mode = "approve"$/m);
  assert.doesNotMatch(installed, /^default_tools_approval_mode = "auto"$/m);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_START"), 1);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_END"), 1);
  assert.equal(countOccurrences(installed, "[mcp_servers.zvec_grep]"), 1);
});

test("Codex installer writes an explicit MCP token environment variable", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-token-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "install",
      "--target",
      "codex",
      "--mcp-transport",
      "http",
      "--mcp-token-env",
      "ZVEC_GREP_SERVER_TOKEN",
      "--yes",
    ],
    {
      env: {
        ...process.env,
        CODEX_HOME: codexHome,
        ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
      },
    },
  );

  const installed = await readFile(join(codexHome, "config.toml"), "utf8");
  assert.match(installed, /^bearer_token_env_var = "ZVEC_GREP_SERVER_TOKEN"$/m);
});

test("Codex uninstaller removes only zvec-grep-managed integration blocks", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-uninstall-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  const agentsPath = join(codexHome, "AGENTS.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(codexHome, { recursive: true });
  await writeFile(configPath, '[mcp_servers.other]\ncommand = "other"\n');
  await writeFile(agentsPath, "# Existing instructions\n");
  await installCodex(codexHome);
  await uninstallCodex(codexHome);
  await uninstallCodex(codexHome);

  const config = await readFile(configPath, "utf8");
  const agents = await readFile(agentsPath, "utf8");
  assert.match(config, /\[mcp_servers\.other\]\ncommand = "other"/);
  assert.doesNotMatch(config, /ZVEC_GREP|mcp_servers\.zvec_grep/);
  assert.match(agents, /# Existing instructions/);
  assert.doesNotMatch(agents, /ZVEC_GREP|## zvec-grep/);
});

test("Codex installer detects and replaces equivalent unmanaged MCP table headers", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-conflict-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const cases = [
    ["leading-whitespace", "  [mcp_servers.zvec_grep]"],
    ["quoted-key", '[mcp_servers."zvec_grep"]'],
  ];

  for (const [name, tableHeader] of cases) {
    const codexHome = join(temporaryDirectory, name);
    const configPath = join(codexHome, "config.toml");
    const existing = [
      "[mcp_servers.other]",
      'command = "other"',
      "",
      tableHeader,
      'command = "old-zg"',
      "",
    ].join("\n");

    await mkdir(codexHome, { recursive: true });
    await writeFile(configPath, existing);

    await assert.rejects(installCodex(codexHome));
    assert.equal(await readFile(configPath, "utf8"), existing);

    await installCodex(codexHome, ["--force"]);

    const installed = await readFile(configPath, "utf8");
    assert.match(installed, /\[mcp_servers\.other\]\ncommand = "other"/);
    assert.doesNotMatch(installed, /old-zg/);
    assert.equal(countOccurrences(installed, "[mcp_servers.zvec_grep]"), 1);
  }
});

test("Codex installer ignores an orphaned end marker before a complete block", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-existing-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(codexHome, { recursive: true });
  await writeFile(
    configPath,
    [
      "[mcp_servers.other]",
      'command = "other"',
      "# ZVEC_GREP_END",
      "",
      "# ZVEC_GREP_START",
      "[mcp_servers.zvec_grep]",
      'command = "old-zg"',
      "# ZVEC_GREP_END",
      "",
    ].join("\n"),
  );

  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "codex", "--yes"],
    {
      env: {
        ...process.env,
        CODEX_HOME: codexHome,
        ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
      },
    },
  );

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]/);
  assert.doesNotMatch(installed, /old-zg/);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_START"), 1);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_END"), 1);
  assert.equal(countOccurrences(installed, "[mcp_servers.zvec_grep]"), 1);
});

test("Codex installer preserves user config after an orphaned start marker", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-orphan-start-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(codexHome, { recursive: true });
  await writeFile(
    configPath,
    [
      "# ZVEC_GREP_START",
      "[mcp_servers.other]",
      'command = "other"',
      "",
      "# ZVEC_GREP_START",
      "[mcp_servers.zvec_grep]",
      'command = "old-zg"',
      "# ZVEC_GREP_END",
      "",
    ].join("\n"),
  );

  await installCodex(codexHome);

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]\ncommand = "other"/);
  assert.doesNotMatch(installed, /old-zg/);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_START"), 1);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_END"), 1);
  assert.equal(countOccurrences(installed, "[mcp_servers.zvec_grep]"), 1);
});

test("Codex installer collapses duplicate managed blocks without deleting config between them", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-duplicate-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(codexHome, { recursive: true });
  await writeFile(
    configPath,
    [
      "# ZVEC_GREP_START",
      "[mcp_servers.zvec_grep]",
      'command = "old-zg-one"',
      "# ZVEC_GREP_END",
      "",
      "[mcp_servers.other]",
      'command = "other"',
      "",
      "# ZVEC_GREP_START",
      "[mcp_servers.zvec_grep]",
      'command = "old-zg-two"',
      "# ZVEC_GREP_END",
      "",
    ].join("\n"),
  );

  await installCodex(codexHome);

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]\ncommand = "other"/);
  assert.doesNotMatch(installed, /old-zg-(?:one|two)/);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_START"), 1);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_END"), 1);
  assert.equal(countOccurrences(installed, "[mcp_servers.zvec_grep]"), 1);
});

test(
  "Codex installer atomically updates symlink targets and preserves their modes",
  {
    skip:
      process.platform === "win32"
        ? "Windows symlink and Unix mode semantics differ"
        : false,
  },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-install-symlink-"),
    );
    const codexHome = join(temporaryDirectory, ".codex");
    const dotfiles = join(temporaryDirectory, "dotfiles");
    const configTarget = join(dotfiles, "config.toml");
    const configPath = join(codexHome, "config.toml");
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    await mkdir(codexHome, { recursive: true });
    await mkdir(dotfiles, { recursive: true });
    await writeFile(configTarget, '[mcp_servers.other]\ncommand = "other"\n');
    await chmod(configTarget, 0o640);
    await symlink(configTarget, configPath);

    await installCodex(codexHome);

    assert.equal((await lstat(configPath)).isSymbolicLink(), true);
    assert.equal((await stat(configTarget)).mode & 0o777, 0o640);
    assert.match(
      await readFile(configTarget, "utf8"),
      /\[mcp_servers\.zvec_grep\]/,
    );
  },
);

test("Codex installer removes temporary files when an atomic replacement fails", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-failure-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(configPath, { recursive: true });

  await assert.rejects(installCodex(codexHome));

  const entries = await readdir(codexHome);
  assert.equal(
    entries.some((entry) => entry.endsWith(".tmp")),
    false,
  );
});

test("Codex installer accepts a custom MCP tool timeout", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-timeout-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installCodex(codexHome, ["--mcp-tool-timeout=900"]);

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /^tool_timeout_sec = 900$/m);
});

test("Codex installer refreshes legacy managed guidance", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-legacy-guidance-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const agentsPath = join(codexHome, "AGENTS.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(codexHome, { recursive: true });
  await writeFile(
    agentsPath,
    [
      "# Existing instructions",
      "",
      "<!-- ZVEC_GREP_START -->",
      "## zvec-grep",
      "legacy guidance",
      "<!-- ZVEC_GREP_END -->",
      "",
    ].join("\n"),
  );

  await installCodex(codexHome);

  const agents = await readFile(agentsPath, "utf8");
  assert.match(agents, /# Existing instructions/);
  assert.match(
    agents,
    /when an exact word, phrase, name, date,[^\n]+use `zvec_grep_rg` when it is listed by the current host; otherwise native Grep or `rg`/i,
  );
  assert.match(
    agents,
    /Use `zvec_grep_search` when wording or location is unknown/,
  );
  assert.match(agents, /comparison or synthesis across files, sections/);
  assert.match(agents, /Choose the evidence source before the retrieval mode/);
  for (const rule of ZVEC_GREP_WORKSPACE_EVIDENCE_RULES) {
    assert.ok(agents.includes(`- ${rule}`));
  }
  assert.match(
    agents,
    /If the index is missing but exact or regex lookup can answer the task, use `zvec_grep_rg` when it is listed by the current host; otherwise native Grep or `rg`/,
  );
  assert.match(
    agents,
    /A workspace may contain any mix of code, documents, configuration, and data/,
  );
  assert.match(agents, /one focused `zvec_grep_search` probe/);
  assert.match(agents, /When no sufficient exact anchor is available/);
  assert.match(agents, /probe does not apply to exact quotations/i);
  assert.match(
    agents,
    /unrelated open-world questions, current external facts/,
  );
  assert.match(agents, /Do not delegate solely to locate material/);
  assert.match(agents, /### Search evidence/);
  assert.doesNotMatch(agents, /### Search arguments and evidence/);
  assert.doesNotMatch(
    agents,
    /`query` creates one primary hybrid result group/,
  );
  assert.doesNotMatch(agents, /"fuse": true/);
  assert.match(agents, /Treat a sufficient snippet as already-read evidence/);
  assert.match(agents, /Creating, rebuilding, or dropping a persistent index/);
  assert.doesNotMatch(agents, /managed-rg/);
  assert.doesNotMatch(agents, /solely to locate code/);
  assert.doesNotMatch(agents, /indexed search first/);
  assert.doesNotMatch(agents, /Indexing and status/);
  assert.doesNotMatch(agents, /Remote data authorization/);
  assert.doesNotMatch(agents, /zg status/);
  assert.doesNotMatch(agents, /legacy guidance/);
});

test("Claude Code installer configures MCP trust and guidance", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-claude-"),
  );
  const claudeConfigDirectory = join(temporaryDirectory, ".claude");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(claudeConfigDirectory, { recursive: true });
  await writeFile(
    join(claudeConfigDirectory, ".claude.json"),
    `${JSON.stringify({ mcpServers: { zvec_grep: { type: "http", url: "http://127.0.0.1:7999/mcp", alwaysLoad: true } } }, null, 2)}\n`,
  );

  const { stdout } = await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "claude", "--yes"],
    {
      env: {
        ...process.env,
        HOME: temporaryDirectory,
        CLAUDE_CONFIG_DIR: claudeConfigDirectory,
        ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
      },
    },
  );

  const mcpConfig = JSON.parse(
    await readFile(join(claudeConfigDirectory, ".claude.json"), "utf8"),
  );
  const settings = JSON.parse(
    await readFile(join(claudeConfigDirectory, "settings.json"), "utf8"),
  );
  const guidance = await readFile(
    join(claudeConfigDirectory, "CLAUDE.md"),
    "utf8",
  );

  assert.deepEqual(mcpConfig.mcpServers.zvec_grep, {
    alwaysLoad: true,
    type: "stdio",
    command: "zg",
    args: ["server", "--stdio"],
  });
  assert.ok(settings.permissions.allow.includes("mcp__zvec_grep__*"));
  assert.match(guidance, /zvec_grep_search/);
  assert.match(guidance, /`zvec_grep_rg` when it is listed/);
  assert.match(
    guidance,
    /when an exact word, phrase, name, date,[^\n]+use `zvec_grep_rg` when it is listed by the current host; otherwise native Grep or `rg`/i,
  );
  assert.doesNotMatch(guidance, /managed-rg/);
  assert.doesNotMatch(guidance, /Indexing and status/);
  assert.doesNotMatch(guidance, /Remote data authorization/);
  assert.doesNotMatch(guidance, /zg status/);
  assert.match(stdout, /zvec-grep setup/);
  assert.match(stdout, /Installing integrations/);
  assert.match(stdout, /Claude Code/);
  assert.doesNotMatch(stdout, /Guidance/);
  assert.doesNotMatch(stdout, /Trust|MCP trust/);
  assert.match(stdout, /Remote data\s+Authorization requested/);
});

test("Claude Code installer preserves user configuration on install and uninstall", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-claude-preserve-"),
  );
  const claudeConfigDirectory = join(temporaryDirectory, ".claude");
  const mcpConfigPath = join(claudeConfigDirectory, ".claude.json");
  const settingsPath = join(claudeConfigDirectory, "settings.json");
  const guidancePath = join(claudeConfigDirectory, "CLAUDE.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(claudeConfigDirectory, { recursive: true });
  await writeFile(
    mcpConfigPath,
    `${JSON.stringify({ mcpServers: { other: { type: "http", url: "https://example.test/mcp" } }, theme: "dark" }, null, 2)}\n`,
  );
  await writeFile(
    settingsPath,
    `${JSON.stringify({ permissions: { allow: ["Bash(git status)"], deny: ["Bash(rm *)"] } }, null, 2)}\n`,
  );
  await writeFile(guidancePath, "# Existing Claude guidance\n");

  const environment = {
    ...process.env,
    HOME: temporaryDirectory,
    CLAUDE_CONFIG_DIR: claudeConfigDirectory,
    ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
  };
  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "claude", "--yes"],
    { env: environment },
  );
  await execFileAsync(
    process.execPath,
    [cliPath, "uninstall", "--target", "claude", "--yes"],
    { env: environment },
  );

  const mcpConfig = JSON.parse(await readFile(mcpConfigPath, "utf8"));
  const settings = JSON.parse(await readFile(settingsPath, "utf8"));
  const guidance = await readFile(guidancePath, "utf8");
  assert.deepEqual(mcpConfig, {
    mcpServers: {
      other: { type: "http", url: "https://example.test/mcp" },
    },
    theme: "dark",
  });
  assert.deepEqual(settings, {
    permissions: {
      allow: ["Bash(git status)"],
      deny: ["Bash(rm *)"],
    },
  });
  assert.match(guidance, /# Existing Claude guidance/);
  assert.doesNotMatch(guidance, /ZVEC_GREP|## zvec-grep/);
});

test("Claude Code installer writes MCP token environment expansion", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-claude-token-"),
  );
  const claudeConfigDirectory = join(temporaryDirectory, ".claude");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "install",
      "--target",
      "claude",
      "--mcp-transport",
      "http",
      "--mcp-token-env",
      "ZVEC_GREP_SERVER_TOKEN",
      "--yes",
    ],
    {
      env: {
        ...process.env,
        HOME: temporaryDirectory,
        CLAUDE_CONFIG_DIR: claudeConfigDirectory,
        ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
      },
    },
  );

  const mcpConfig = JSON.parse(
    await readFile(join(claudeConfigDirectory, ".claude.json"), "utf8"),
  );
  assert.equal(
    mcpConfig.mcpServers.zvec_grep.headers.Authorization,
    "Bearer ${ZVEC_GREP_SERVER_TOKEN}",
  );
});

test("Claude Code installer accepts cc and claude-code compatibility aliases", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-claude-aliases-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  for (const alias of ["cc", "claude-code"]) {
    const configDirectory = join(temporaryDirectory, alias);
    await installTarget(alias, { CLAUDE_CONFIG_DIR: configDirectory });
    const config = JSON.parse(
      await readFile(join(configDirectory, ".claude.json"), "utf8"),
    );
    assert.equal(config.mcpServers.zvec_grep.command, "zg");
    assert.deepEqual(config.mcpServers.zvec_grep.args, ["server", "--stdio"]);
  }
});

test("Qwen Code installer accepts qwen aliases and numeric target 5", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-aliases-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  for (const target of ["qwen", "qwen-code", "qwencode", "5"]) {
    const qwenHome = join(temporaryDirectory, target);
    await installTarget(target, { QWEN_HOME: qwenHome });
    const config = JSON.parse(
      await readFile(join(qwenHome, "settings.json"), "utf8"),
    );
    assert.equal(config.mcpServers.zvec_grep.command, "zg");
    assert.deepEqual(config.mcpServers.zvec_grep.args, ["server", "--stdio"]);
  }
});

test("Qwen Code installer configures full stdio tools, trust, timeout, and guidance", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-stdio-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget("qwen", { QWEN_HOME: qwenHome }, [
    "--mcp-toolset=full",
    "--mcp-tool-timeout=900",
  ]);

  const config = JSON.parse(
    await readFile(join(qwenHome, "settings.json"), "utf8"),
  );
  assert.deepEqual(config.mcpServers.zvec_grep, {
    command: "zg",
    args: ["server", "--stdio", "--mcp-toolset", "full"],
    timeout: 900000,
    alwaysLoadTools: true,
    trust: true,
  });
  assert.equal(config.permissions, undefined);

  const guidance = await readFile(join(qwenHome, "QWEN.md"), "utf8");
  assert.match(guidance, /`mcp__zvec_grep__zvec_grep_search`/);
  assert.match(guidance, /`mcp__zvec_grep__zvec_grep_rg`/);
  assert.equal(countOccurrences(guidance, "<!-- ZVEC_GREP_START -->"), 1);
  assert.equal(countOccurrences(guidance, "<!-- ZVEC_GREP_END -->"), 1);
});

test("Qwen Code installer writes Streamable HTTP configuration and token expansion", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-http-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget("qwen", { QWEN_HOME: qwenHome }, [
    "--mcp-transport=http",
    "--mcp-tool-timeout=42",
    "--mcp-token-env=ZVEC_GREP_SERVER_TOKEN",
  ]);

  const config = JSON.parse(
    await readFile(join(qwenHome, "settings.json"), "utf8"),
  );
  assert.deepEqual(config.mcpServers.zvec_grep, {
    httpUrl: "http://127.0.0.1:7999/mcp",
    timeout: 42000,
    alwaysLoadTools: true,
    trust: true,
    headers: {
      Authorization: "Bearer ${ZVEC_GREP_SERVER_TOKEN}",
    },
  });
  assert.equal(config.mcpServers.zvec_grep.url, undefined);
  assert.equal(config.mcpServers.zvec_grep.command, undefined);
  assert.equal(config.permissions, undefined);
});

test("Qwen Code installer preserves comments and rejects trailing commas", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-jsonc-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  const configPath = join(qwenHome, "settings.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qwenHome, { recursive: true });
  await writeFile(
    configPath,
    [
      "{",
      "  // Keep the user's theme comment.",
      '  "theme": "dark",',
      "  /* Keep the other MCP server comment. */",
      '  "mcpServers": {',
      '    "other": { "httpUrl": "https://example.test/mcp" }',
      "  }",
      "}",
      "",
    ].join("\n"),
  );

  await installTarget("qwen", { QWEN_HOME: qwenHome });
  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\/\/ Keep the user's theme comment\./);
  assert.match(installed, /\/\* Keep the other MCP server comment\. \*\//);
  assert.match(installed, /"theme"\s*:\s*"dark"/);
  assert.match(installed, /"other"\s*:\s*\{[^}]*example\.test\/mcp/s);
  assert.match(installed, /"command"\s*:\s*"zg"/);

  const invalidHome = join(temporaryDirectory, "invalid");
  const invalidConfigPath = join(invalidHome, "settings.json");
  const invalidSource = '{\n  "theme": "dark",\n}\n';
  await mkdir(invalidHome, { recursive: true });
  await writeFile(invalidConfigPath, invalidSource);

  await assert.rejects(
    installTarget("qwen", { QWEN_HOME: invalidHome }),
    /settings\.json|Qwen|JSON/i,
  );
  assert.equal(await readFile(invalidConfigPath, "utf8"), invalidSource);
  await assert.rejects(stat(join(invalidHome, "QWEN.md")), {
    code: "ENOENT",
  });
});

test("Qwen Code installer requires force for unmanaged servers and force replaces them cleanly", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-conflict-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  const configPath = join(qwenHome, "settings.json");
  const original = `${JSON.stringify(
    {
      theme: "dark",
      mcpServers: {
        zvec_grep: {
          httpUrl: "https://example.test/foreign-mcp",
          trust: false,
          description: "user-owned server",
        },
      },
    },
    null,
    2,
  )}\n`;
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qwenHome, { recursive: true });
  await writeFile(configPath, original);
  await assert.rejects(
    installTarget("qwen", { QWEN_HOME: qwenHome }),
    /--force/,
  );
  assert.equal(await readFile(configPath, "utf8"), original);
  await assert.rejects(stat(join(qwenHome, "QWEN.md")), { code: "ENOENT" });

  await installTarget("qwen", { QWEN_HOME: qwenHome }, ["--force"]);
  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(config.theme, "dark");
  assert.deepEqual(config.mcpServers.zvec_grep, {
    command: "zg",
    args: ["server", "--stdio"],
    timeout: 600000,
    alwaysLoadTools: true,
    trust: true,
  });
  assert.equal(config.mcpServers.zvec_grep.description, undefined);
});

test("Qwen Code installer replaces its managed server entry cleanly", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-policy-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  const configPath = join(qwenHome, "settings.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qwenHome, { recursive: true });
  await writeFile(
    configPath,
    `${JSON.stringify(
      {
        mcpServers: {
          zvec_grep: {
            command: "zg",
            args: ["server", "--stdio"],
            timeout: 1000,
            headers: { "X-Old": "remove me" },
            trust: false,
            description: "Keep this policy",
            includeTools: ["zvec_grep_search"],
            excludeTools: ["zvec_grep_drop"],
            discoveryTimeoutMs: 3210,
            alwaysLoadTools: false,
          },
        },
        permissions: { allow: ["Bash(git status)"] },
      },
      null,
      2,
    )}\n`,
  );

  await installTarget("qwen", { QWEN_HOME: qwenHome }, [
    "--mcp-transport=http",
    "--mcp-toolset=full",
    "--mcp-token-env=ZVEC_GREP_SERVER_TOKEN",
  ]);

  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.deepEqual(config.mcpServers.zvec_grep, {
    httpUrl: "http://127.0.0.1:7999/mcp",
    timeout: 600000,
    alwaysLoadTools: true,
    trust: true,
    headers: {
      Authorization: "Bearer ${ZVEC_GREP_SERVER_TOKEN}",
    },
  });
  assert.deepEqual(config.permissions, { allow: ["Bash(git status)"] });
});

test("Qwen Code install and uninstall are idempotent and preserve user content", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-idempotent-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  const configPath = join(qwenHome, "settings.json");
  const guidancePath = join(qwenHome, "QWEN.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qwenHome, { recursive: true });
  await writeFile(
    configPath,
    `${JSON.stringify(
      {
        theme: "dark",
        mcpServers: {
          other: { httpUrl: "https://example.test/mcp" },
        },
        permissions: {
          allow: ["Bash(git status)"],
          deny: ["Bash(rm *)"],
        },
      },
      null,
      2,
    )}\n`,
  );
  await writeFile(guidancePath, "# Existing Qwen guidance\n");

  await installTarget("qwen", { QWEN_HOME: qwenHome }, ["--mcp-toolset=full"]);
  const firstInstallConfig = await readFile(configPath, "utf8");
  const firstInstallGuidance = await readFile(guidancePath, "utf8");
  assert.deepEqual(JSON.parse(firstInstallConfig).permissions, {
    allow: ["Bash(git status)"],
    deny: ["Bash(rm *)"],
  });
  await installTarget("qwen", { QWEN_HOME: qwenHome }, ["--mcp-toolset=full"]);
  assert.equal(await readFile(configPath, "utf8"), firstInstallConfig);
  assert.equal(await readFile(guidancePath, "utf8"), firstInstallGuidance);

  await uninstallTarget("qwen", { QWEN_HOME: qwenHome });
  const firstUninstallConfig = await readFile(configPath, "utf8");
  const firstUninstallGuidance = await readFile(guidancePath, "utf8");
  await uninstallTarget("qwen", { QWEN_HOME: qwenHome });
  assert.equal(await readFile(configPath, "utf8"), firstUninstallConfig);
  assert.equal(await readFile(guidancePath, "utf8"), firstUninstallGuidance);

  const config = JSON.parse(firstUninstallConfig);
  assert.equal(config.theme, "dark");
  assert.equal(config.mcpServers.zvec_grep, undefined);
  assert.equal(config.mcpServers.other.httpUrl, "https://example.test/mcp");
  assert.deepEqual(config.permissions, {
    allow: ["Bash(git status)"],
    deny: ["Bash(rm *)"],
  });
  assert.match(firstUninstallGuidance, /# Existing Qwen guidance/);
  assert.doesNotMatch(firstUninstallGuidance, /ZVEC_GREP|## zvec-grep/);
});

test("Qwen Code installer and uninstaller preserve arbitrary permissions", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-permissions-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  const configPath = join(qwenHome, "settings.json");
  const permissions = {
    allow: ["Bash(git status)", "mcp__zvec_grep__zvec_grep_search"],
    ask: ["mcp__zvec_grep__zvec_grep_drop"],
    deny: ["Bash(rm *)"],
    customPolicy: { nested: ["keep", "unchanged"] },
  };
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qwenHome, { recursive: true });
  await writeFile(
    configPath,
    `${JSON.stringify({ theme: "dark", permissions }, null, 2)}\n`,
  );

  await installTarget("qwen", { QWEN_HOME: qwenHome });
  const installed = JSON.parse(await readFile(configPath, "utf8"));
  assert.deepEqual(installed.permissions, permissions);
  assert.equal(installed.mcpServers.zvec_grep.trust, true);

  await uninstallTarget("qwen", { QWEN_HOME: qwenHome });
  const uninstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.deepEqual(uninstalled.permissions, permissions);
  assert.equal(uninstalled.mcpServers, undefined);
  assert.equal(uninstalled.theme, "dark");
});

test("Qwen Code installer warns when context.fileName excludes QWEN.md", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-context-"),
  );
  const qwenHome = join(temporaryDirectory, ".qwen");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qwenHome, { recursive: true });
  await writeFile(
    join(qwenHome, "settings.json"),
    '{"context":{"fileName":["CONTEXT.md"]}}\n',
  );
  const { stdout, stderr } = await installTarget("qwen", {
    QWEN_HOME: qwenHome,
  });
  const output = `${stdout}\n${stderr}`;
  assert.match(output, /QWEN\.md/i);
  assert.match(output, /context\.fileName|not load|exclud/i);
  await stat(join(qwenHome, "QWEN.md"));
});

test("Qwen Code installer resolves QWEN_HOME and dotenv fallbacks", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qwen-home-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const dotenvHome = join(temporaryDirectory, "dotenv-home");
  const redirectedFromQwenEnv = join(temporaryDirectory, "qwen-env-target");
  const redirectedFromHomeEnv = join(temporaryDirectory, "home-env-target");
  await mkdir(join(dotenvHome, ".qwen"), { recursive: true });
  await writeFile(
    join(dotenvHome, ".qwen", ".env"),
    `QWEN_HOME=${redirectedFromQwenEnv}\n`,
  );
  await writeFile(
    join(dotenvHome, ".env"),
    `QWEN_HOME=${redirectedFromHomeEnv}\n`,
  );
  await installTarget("qwen", {
    HOME: dotenvHome,
    USERPROFILE: dotenvHome,
    QWEN_HOME: undefined,
  });
  await stat(join(redirectedFromQwenEnv, "settings.json"));
  await assert.rejects(stat(join(redirectedFromHomeEnv, "settings.json")), {
    code: "ENOENT",
  });

  const homeEnvHome = join(temporaryDirectory, "home-env-home");
  const redirectedFromOnlyHomeEnv = join(
    temporaryDirectory,
    "only-home-env-target",
  );
  await mkdir(homeEnvHome, { recursive: true });
  await writeFile(
    join(homeEnvHome, ".env"),
    `QWEN_HOME=${redirectedFromOnlyHomeEnv}\n`,
  );
  await installTarget("qwen", {
    HOME: homeEnvHome,
    USERPROFILE: homeEnvHome,
    QWEN_HOME: undefined,
  });
  await stat(join(redirectedFromOnlyHomeEnv, "settings.json"));

  const emptyValueHome = join(temporaryDirectory, "empty-value-home");
  const ignoredRedirect = join(temporaryDirectory, "ignored-redirect");
  await mkdir(join(emptyValueHome, ".qwen"), { recursive: true });
  await writeFile(
    join(emptyValueHome, ".qwen", ".env"),
    `QWEN_HOME=${ignoredRedirect}\n`,
  );
  await installTarget("qwen", {
    HOME: emptyValueHome,
    USERPROFILE: emptyValueHome,
    QWEN_HOME: "",
  });
  await stat(join(emptyValueHome, ".qwen", "settings.json"));
  await assert.rejects(stat(join(ignoredRedirect, "settings.json")), {
    code: "ENOENT",
  });
});

test("Qoder installer accepts qoder aliases and numeric target 6", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-aliases-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  for (const target of ["qoder", "qodercli", "qoder-cli", "6"]) {
    const qoderConfigDirectory = join(temporaryDirectory, target);
    const qoderProjectDirectory = join(temporaryDirectory, `${target}-project`);
    await installTarget(target, {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    });
    const config = JSON.parse(
      await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
    );
    assert.equal(config.mcpServers.zvec_grep.command, process.execPath);
    assert.deepEqual(config.mcpServers.zvec_grep.args, stableStdioArgs());
    assert.equal(
      config.mcpServers.zvec_grep.description,
      "Managed by zg install",
    );
    await stat(join(qoderProjectDirectory, ".qoder", "rules", "zvec-grep.md"));
  }
});

test("Qoder installer uses its working directory as the IDE project root", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-git-root-"),
  );
  const projectRoot = join(temporaryDirectory, "project");
  const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
  const environment = {
    ...process.env,
    QODER_CONFIG_DIR: qoderConfigDirectory,
    ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
  };
  delete environment.QODER_PROJECT_DIR;
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(projectRoot, { recursive: true });
  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "qoder", "--yes"],
    { cwd: projectRoot, env: environment },
  );

  const rule = await readFile(
    join(projectRoot, ".qoder", "rules", "zvec-grep.md"),
    "utf8",
  );
  assert.match(rule, /^---\ntrigger: always_on\n---\n/);
});

test("Qoder installer preserves JSONC comments and writes trusted stdio config and guidance", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-stdio-"),
  );
  const qoderConfigDirectory = join(temporaryDirectory, ".qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  const configPath = join(qoderConfigDirectory, "settings.json");
  const guidancePath = join(qoderConfigDirectory, "AGENTS.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qoderConfigDirectory, { recursive: true });
  await writeFile(
    configPath,
    [
      "{",
      "  // Keep the user's theme comment.",
      '  "theme": "dark",',
      "  /* Keep the other MCP server comment. */",
      '  "mcpServers": {',
      '    "other": { "type": "http", "url": "https://example.test/mcp" }',
      "  }",
      "}",
      "",
    ].join("\n"),
  );
  await writeFile(guidancePath, "# Existing Qoder guidance\n");

  await installTarget(
    "qoder",
    {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    },
    ["--mcp-toolset=full", "--mcp-tool-timeout=900"],
  );

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\/\/ Keep the user's theme comment\./);
  assert.match(installed, /\/\* Keep the other MCP server comment\. \*\//);
  const config = parseJsonWithComments(installed);
  assert.equal(config.theme, "dark");
  assert.equal(config.mcpServers.other.url, "https://example.test/mcp");
  assert.deepEqual(config.mcpServers.zvec_grep, {
    command: process.execPath,
    args: stableStdioArgs(["--mcp-toolset", "full"]),
    timeout: 900000,
    trust: true,
    description: "Managed by zg install",
  });

  const guidance = await readFile(guidancePath, "utf8");
  assert.match(guidance, /# Existing Qoder guidance/);
  assert.match(guidance, /`mcp__zvec_grep__zvec_grep_search`/);
  assert.match(guidance, /`mcp__zvec_grep__zvec_grep_rg`/);
  assert.match(guidance, /### Qoder Remote Embedding authorization recovery/);
  assert.match(guidance, /no handler registered for `elicitation\/create`/);
  assert.match(
    guidance,
    /authorization was declined or cancelled without showing the user an authorization form/,
  );
  assert.match(
    guidance,
    /Use `AskUserQuestion` to offer exactly these choices/,
  );
  assert.match(
    guidance,
    /zg auth grant "<absolute-root>" --capability embedding --scope workspace/,
  );
  assert.match(guidance, /retry the original search call once/);
  assert.match(
    guidance,
    /original search text in `fts`, omit `query`, `queries`, and `vector`, set `autoUpdate` to `false` and `freshness` to `eventual`/,
  );
  assert.match(guidance, /interactive user input is unavailable, stop/);
  assert.match(
    guidance,
    /never request or modify an API key merely to resolve this interaction error/,
  );
  assert.equal(countOccurrences(guidance, "<!-- ZVEC_GREP_START -->"), 1);
  assert.equal(countOccurrences(guidance, "<!-- ZVEC_GREP_END -->"), 1);
});

test("Codex and Qoder stable stdio launches work with an empty PATH", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-stable-stdio-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installCodex(codexHome);
  await installTarget("qoder", {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  });

  const codexLaunch = parseCodexStdioLaunch(
    await readFile(join(codexHome, "config.toml"), "utf8"),
  );
  const qoderConfig = JSON.parse(
    await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
  );
  const qoderLaunch = qoderConfig.mcpServers.zvec_grep;

  for (const launch of [codexLaunch, qoderLaunch]) {
    assert.equal(launch.command, process.execPath);
    assert.equal(launch.args[0], cliPath);
    const { stdout } = await execFileAsync(
      launch.command,
      [launch.args[0], "version"],
      { env: { ...process.env, PATH: "" } },
    );
    assert.match(stdout, /^0\.2\.0\s*$/);
  }
});

test("Qoder IDE rule install and uninstall are idempotent and preserve user content", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-ide-rule-"),
  );
  const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  const projectQoderDirectory = join(qoderProjectDirectory, ".qoder");
  const rulePath = join(projectQoderDirectory, "rules", "zvec-grep.md");
  const siblingSettingsPath = join(
    projectQoderDirectory,
    "settings.local.json",
  );
  const siblingSettings = [
    "{",
    "  // This project setting is owned by the user.",
    '  "chat.language": "zh-CN"',
    "}",
    "",
  ].join("\n");
  const environment = {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  };
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(projectQoderDirectory, { recursive: true });
  await writeFile(siblingSettingsPath, siblingSettings);

  await installTarget("qoder", environment);
  const firstConfig = await readFile(
    join(qoderConfigDirectory, "settings.json"),
    "utf8",
  );
  const firstGlobalGuidance = await readFile(
    join(qoderConfigDirectory, "AGENTS.md"),
    "utf8",
  );
  const firstRule = await readFile(rulePath, "utf8");
  assert.match(firstRule, /^---\ntrigger: always_on\n---\n/);
  assert.equal(countOccurrences(firstRule, "<!-- ZVEC_GREP_START -->"), 1);
  assert.equal(countOccurrences(firstRule, "<!-- ZVEC_GREP_END -->"), 1);
  assert.equal(await readFile(siblingSettingsPath, "utf8"), siblingSettings);

  await installTarget("qoder", environment);
  assert.equal(
    await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
    firstConfig,
  );
  assert.equal(
    await readFile(join(qoderConfigDirectory, "AGENTS.md"), "utf8"),
    firstGlobalGuidance,
  );
  assert.equal(await readFile(rulePath, "utf8"), firstRule);

  const userRuleContent = "\n# User project note\nKeep this project rule.\n";
  await writeFile(rulePath, `${firstRule}${userRuleContent}`);
  const customizedRule = await readFile(rulePath, "utf8");
  assert.match(customizedRule, /# User project note\nKeep this project rule\./);
  assert.equal(await readFile(siblingSettingsPath, "utf8"), siblingSettings);

  await uninstallTarget("qoder", environment);
  const firstUninstallRule = await readFile(rulePath, "utf8");
  const firstUninstallSibling = await readFile(siblingSettingsPath, "utf8");
  await uninstallTarget("qoder", environment);
  assert.equal(await readFile(rulePath, "utf8"), firstUninstallRule);
  assert.equal(
    await readFile(siblingSettingsPath, "utf8"),
    firstUninstallSibling,
  );
  assert.match(
    firstUninstallRule,
    /# User project note\nKeep this project rule\./,
  );
  assert.doesNotMatch(firstUninstallRule, /ZVEC_GREP|## zvec-grep/);
  assert.equal(firstUninstallSibling, siblingSettings);
});

test("Qoder IDE installer requires force before replacing an unmanaged same-name rule", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-ide-conflict-"),
  );
  const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  const rulePath = join(
    qoderProjectDirectory,
    ".qoder",
    "rules",
    "zvec-grep.md",
  );
  const originalRule =
    "# User-owned zvec-grep rule\nDo not replace silently.\n";
  const environment = {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  };
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(join(qoderProjectDirectory, ".qoder", "rules"), {
    recursive: true,
  });
  await writeFile(rulePath, originalRule);

  await assert.rejects(
    installTarget("qoder", environment),
    /zvec-grep\.md|--force/i,
  );
  assert.equal(await readFile(rulePath, "utf8"), originalRule);
  await assert.rejects(stat(join(qoderConfigDirectory, "settings.json")), {
    code: "ENOENT",
  });

  await installTarget("qoder", environment, ["--force"]);
  const installedRule = await readFile(rulePath, "utf8");
  assert.match(installedRule, /^---\ntrigger: always_on\n---\n/);
  assert.match(installedRule, /<!-- ZVEC_GREP_START -->/);
  assert.doesNotMatch(installedRule, /User-owned zvec-grep rule/);
});

test(
  "Qoder IDE force replaces a direct rule symlink without modifying its target",
  {
    skip:
      process.platform === "win32" ? "Windows symlink semantics differ" : false,
  },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-install-qoder-rule-symlink-"),
    );
    const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
    const qoderProjectDirectory = join(temporaryDirectory, "project");
    const ruleDirectory = join(qoderProjectDirectory, ".qoder", "rules");
    const rulePath = join(ruleDirectory, "zvec-grep.md");
    const externalRule = join(temporaryDirectory, "user-owned-rule.md");
    const externalContent = "# User-owned external rule\n";
    const environment = {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    };
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    await mkdir(ruleDirectory, { recursive: true });
    await writeFile(externalRule, externalContent);
    await symlink(externalRule, rulePath);

    await assert.rejects(
      installTarget("qoder", environment),
      /symbolic link|--force/i,
    );
    assert.equal(await readFile(externalRule, "utf8"), externalContent);
    assert.equal((await lstat(rulePath)).isSymbolicLink(), true);

    await installTarget("qoder", environment, ["--force"]);
    assert.equal(await readFile(externalRule, "utf8"), externalContent);
    assert.equal((await lstat(rulePath)).isSymbolicLink(), false);
    const installedRule = await readFile(rulePath, "utf8");
    const installedConfig = await readFile(
      join(qoderConfigDirectory, "settings.json"),
      "utf8",
    );
    const installedGuidance = await readFile(
      join(qoderConfigDirectory, "AGENTS.md"),
      "utf8",
    );
    assert.match(installedRule, /## zvec-grep/);

    await rm(rulePath, { force: true });
    await symlink(externalRule, rulePath);
    await assert.rejects(
      uninstallTarget("qoder", environment),
      /symbolic link/i,
    );
    assert.equal(
      await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
      installedConfig,
    );
    assert.equal(
      await readFile(join(qoderConfigDirectory, "AGENTS.md"), "utf8"),
      installedGuidance,
    );
    assert.equal(await readFile(externalRule, "utf8"), externalContent);

    await rm(rulePath, { force: true });
    await writeFile(rulePath, installedRule);
    await uninstallTarget("qoder", environment);
    assert.equal(await readFile(externalRule, "utf8"), externalContent);
    await assert.rejects(stat(rulePath), { code: "ENOENT" });
  },
);

test(
  "Qoder IDE installer and uninstaller refuse a symlinked rules directory",
  {
    skip:
      process.platform === "win32" ? "Windows symlink semantics differ" : false,
  },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-install-qoder-rules-symlink-"),
    );
    const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
    const qoderProjectDirectory = join(temporaryDirectory, "project");
    const projectQoderDirectory = join(qoderProjectDirectory, ".qoder");
    const externalRulesDirectory = join(temporaryDirectory, "external-rules");
    const externalRulePath = join(externalRulesDirectory, "zvec-grep.md");
    const environment = {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    };
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    await mkdir(projectQoderDirectory, { recursive: true });
    await mkdir(externalRulesDirectory, { recursive: true });
    await symlink(externalRulesDirectory, join(projectQoderDirectory, "rules"));

    await assert.rejects(
      installTarget("qoder", environment, ["--force"]),
      /symbolic-link directory/i,
    );
    await assert.rejects(stat(externalRulePath), { code: "ENOENT" });
    await assert.rejects(stat(join(qoderConfigDirectory, "settings.json")), {
      code: "ENOENT",
    });

    await rm(join(projectQoderDirectory, "rules"), { force: true });
    await installTarget("qoder", environment);
    const installedConfig = await readFile(
      join(qoderConfigDirectory, "settings.json"),
      "utf8",
    );
    const installedGuidance = await readFile(
      join(qoderConfigDirectory, "AGENTS.md"),
      "utf8",
    );
    await rm(join(projectQoderDirectory, "rules"), {
      recursive: true,
      force: true,
    });
    await symlink(externalRulesDirectory, join(projectQoderDirectory, "rules"));

    await assert.rejects(
      uninstallTarget("qoder", environment),
      /symbolic-link directory/i,
    );
    assert.equal(
      await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
      installedConfig,
    );
    assert.equal(
      await readFile(join(qoderConfigDirectory, "AGENTS.md"), "utf8"),
      installedGuidance,
    );
    await assert.rejects(stat(externalRulePath), { code: "ENOENT" });
  },
);

test("Qoder uninstaller preserves comments around a first or only managed server", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-uninstall-qoder-comments-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const firstConfigDirectory = join(temporaryDirectory, "first");
  const firstProjectDirectory = join(temporaryDirectory, "first-project");
  const firstConfigPath = join(firstConfigDirectory, "settings.json");
  await mkdir(firstConfigDirectory, { recursive: true });
  await writeFile(
    firstConfigPath,
    [
      "{",
      '  "mcpServers": {',
      "    // Keep the container note.",
      '    "zvec_grep": { "command": "zg", "args": ["server", "--stdio"] } /* Keep the separator, note. */,',
      "    // Keep the other server note.",
      '    "other": { "type": "http", "url": "https://example.test/mcp" }',
      "  }",
      "}",
      "",
    ].join("\n"),
  );

  await uninstallTarget("qoder", {
    QODER_CONFIG_DIR: firstConfigDirectory,
    QODER_PROJECT_DIR: firstProjectDirectory,
  });
  const firstUninstall = await readFile(firstConfigPath, "utf8");
  assert.match(firstUninstall, /\/\/ Keep the container note\./);
  assert.match(firstUninstall, /\/\* Keep the separator, note\. \*\//);
  assert.match(firstUninstall, /\/\/ Keep the other server note\./);
  const firstConfig = parseJsonWithComments(firstUninstall);
  assert.equal(firstConfig.mcpServers.zvec_grep, undefined);
  assert.equal(firstConfig.mcpServers.other.url, "https://example.test/mcp");

  const onlyConfigDirectory = join(temporaryDirectory, "only");
  const onlyProjectDirectory = join(temporaryDirectory, "only-project");
  const onlyConfigPath = join(onlyConfigDirectory, "settings.json");
  await mkdir(onlyConfigDirectory, { recursive: true });
  await writeFile(
    onlyConfigPath,
    [
      "{",
      '  "mcpServers": {',
      "    // Keep this user note even when the managed server is removed.",
      '    "zvec_grep": { "command": "zg", "args": ["server", "--stdio"] }',
      "  },",
      '  "theme": "dark"',
      "}",
      "",
    ].join("\n"),
  );

  await uninstallTarget("qoder", {
    QODER_CONFIG_DIR: onlyConfigDirectory,
    QODER_PROJECT_DIR: onlyProjectDirectory,
  });
  const onlyUninstall = await readFile(onlyConfigPath, "utf8");
  assert.match(
    onlyUninstall,
    /\/\/ Keep this user note even when the managed server is removed\./,
  );
  const onlyConfig = parseJsonWithComments(onlyUninstall);
  assert.deepEqual(onlyConfig.mcpServers, {});
  assert.equal(onlyConfig.theme, "dark");
});

test("Qoder installer writes trusted HTTP configuration and token expansion", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-http-"),
  );
  const qoderConfigDirectory = join(temporaryDirectory, ".qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget(
    "qoder",
    {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    },
    [
      "--mcp-transport=http",
      "--mcp-tool-timeout=42",
      "--mcp-token-env=ZVEC_GREP_SERVER_TOKEN",
    ],
  );

  const config = JSON.parse(
    await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
  );
  assert.deepEqual(config.mcpServers.zvec_grep, {
    type: "http",
    url: "http://127.0.0.1:7999/mcp",
    timeout: 42000,
    trust: true,
    description: "Managed by zg install",
    headers: {
      Authorization: "Bearer ${ZVEC_GREP_SERVER_TOKEN}",
    },
  });
});

test("Qoder recognizes its managed HTTP server after the configured URL changes", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-http-url-change-"),
  );
  const qoderConfigDirectory = join(temporaryDirectory, "global-qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  const configPath = join(qoderConfigDirectory, "settings.json");
  const baseEnvironment = {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  };
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget(
    "qoder",
    {
      ...baseEnvironment,
      ZVEC_GREP_SERVER_URL: "http://127.0.0.1:8101/mcp",
    },
    ["--mcp-transport=http"],
  );
  await installTarget(
    "qoder",
    {
      ...baseEnvironment,
      ZVEC_GREP_SERVER_URL: "http://127.0.0.1:8102/mcp",
    },
    ["--mcp-transport=http"],
  );

  const reinstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(
    reinstalled.mcpServers.zvec_grep.url,
    "http://127.0.0.1:8102/mcp",
  );
  await uninstallTarget("qoder", {
    ...baseEnvironment,
    ZVEC_GREP_SERVER_URL: "http://127.0.0.1:8103/mcp",
  });
  const uninstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(uninstalled.mcpServers?.zvec_grep, undefined);
});

test("Qoder installer requires force and uninstall is managed and idempotent", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-conflict-"),
  );
  const qoderConfigDirectory = join(temporaryDirectory, ".qoder");
  const qoderProjectDirectory = join(temporaryDirectory, "project");
  const configPath = join(qoderConfigDirectory, "settings.json");
  const guidancePath = join(qoderConfigDirectory, "AGENTS.md");
  const original = `${JSON.stringify(
    {
      theme: "dark",
      mcpServers: {
        other: { type: "http", url: "https://example.test/mcp" },
        zvec_grep: {
          type: "http",
          url: "https://example.test/user-owned-mcp",
          trust: false,
        },
      },
    },
    null,
    2,
  )}\n`;
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(qoderConfigDirectory, { recursive: true });
  await writeFile(configPath, original);
  await writeFile(guidancePath, "# Existing Qoder guidance\n");

  await assert.rejects(
    installTarget("qoder", {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    }),
    /--force/,
  );
  assert.equal(await readFile(configPath, "utf8"), original);

  await uninstallTarget("qoder", {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  });
  assert.equal(await readFile(configPath, "utf8"), original);
  assert.equal(
    await readFile(guidancePath, "utf8"),
    "# Existing Qoder guidance\n",
  );

  await installTarget(
    "qoder",
    {
      QODER_CONFIG_DIR: qoderConfigDirectory,
      QODER_PROJECT_DIR: qoderProjectDirectory,
    },
    ["--force"],
  );
  await uninstallTarget("qoder", {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  });
  const firstUninstallConfig = await readFile(configPath, "utf8");
  const firstUninstallGuidance = await readFile(guidancePath, "utf8");
  await uninstallTarget("qoder", {
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: qoderProjectDirectory,
  });
  assert.equal(await readFile(configPath, "utf8"), firstUninstallConfig);
  assert.equal(await readFile(guidancePath, "utf8"), firstUninstallGuidance);

  const config = JSON.parse(firstUninstallConfig);
  assert.equal(config.theme, "dark");
  assert.equal(config.mcpServers.zvec_grep, undefined);
  assert.equal(config.mcpServers.other.url, "https://example.test/mcp");
  assert.match(firstUninstallGuidance, /# Existing Qoder guidance/);
  assert.doesNotMatch(firstUninstallGuidance, /ZVEC_GREP|## zvec-grep/);
});

test("Qoder installer uses the default home and honors QODER_CONFIG_DIR", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-qoder-home-"),
  );
  const defaultHome = join(temporaryDirectory, "default-home");
  const qoderConfigDirectory = join(temporaryDirectory, "custom-qoder-home");
  const defaultProjectDirectory = join(temporaryDirectory, "default-project");
  const customProjectDirectory = join(temporaryDirectory, "custom-project");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget("qoder", {
    HOME: defaultHome,
    USERPROFILE: defaultHome,
    QODER_CONFIG_DIR: undefined,
    QODER_PROJECT_DIR: defaultProjectDirectory,
  });
  await stat(join(defaultHome, ".qoder", "settings.json"));
  await stat(join(defaultProjectDirectory, ".qoder", "rules", "zvec-grep.md"));

  await installTarget("qoder", {
    HOME: temporaryDirectory,
    USERPROFILE: temporaryDirectory,
    QODER_CONFIG_DIR: qoderConfigDirectory,
    QODER_PROJECT_DIR: customProjectDirectory,
  });

  await stat(join(qoderConfigDirectory, "settings.json"));
  await stat(join(qoderConfigDirectory, "AGENTS.md"));
  await stat(join(customProjectDirectory, ".qoder", "rules", "zvec-grep.md"));
  await assert.rejects(
    stat(join(temporaryDirectory, ".qoder", "settings.json")),
    { code: "ENOENT" },
  );
});

test(
  "auto target detects either qoder executable",
  {
    skip:
      process.platform === "win32" ? "PATH executable semantics differ" : false,
  },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-install-auto-qoder-"),
    );
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    for (const executable of ["qoder", "qodercli"]) {
      const caseDirectory = join(temporaryDirectory, executable);
      const binaryDirectory = join(caseDirectory, "bin");
      const qoderConfigDirectory = join(caseDirectory, ".qoder");
      const qoderProjectDirectory = join(caseDirectory, "project");
      await mkdir(binaryDirectory, { recursive: true });
      const executablePath = join(binaryDirectory, executable);
      await writeFile(executablePath, "#!/bin/sh\n");
      await chmod(executablePath, 0o755);

      const { stdout } = await execFileAsync(
        process.execPath,
        [cliPath, "install", "--yes"],
        {
          env: {
            ...process.env,
            PATH: binaryDirectory,
            HOME: caseDirectory,
            USERPROFILE: caseDirectory,
            QODER_CONFIG_DIR: qoderConfigDirectory,
            QODER_PROJECT_DIR: qoderProjectDirectory,
            ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
          },
        },
      );

      assert.match(stdout, /Qoder/);
      assert.doesNotMatch(
        stdout,
        /Claude Code|Codex|OpenCode|Cursor|Qwen Code/,
      );
      const config = JSON.parse(
        await readFile(join(qoderConfigDirectory, "settings.json"), "utf8"),
      );
      assert.equal(config.mcpServers.zvec_grep.command, process.execPath);
      assert.deepEqual(config.mcpServers.zvec_grep.args, stableStdioArgs());
      assert.equal(
        config.mcpServers.zvec_grep.description,
        "Managed by zg install",
      );
    }
  },
);

test("Cursor installer manages a global Streamable HTTP MCP server", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-cursor-"),
  );
  const cursorConfigDirectory = join(temporaryDirectory, ".cursor");
  const configPath = join(cursorConfigDirectory, "mcp.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget("cursor", { CURSOR_CONFIG_DIR: cursorConfigDirectory }, [
    "--mcp-transport",
    "http",
    "--mcp-token-env",
    "ZVEC_GREP_SERVER_TOKEN",
  ]);

  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.deepEqual(config.mcpServers.zvec_grep, {
    url: "http://127.0.0.1:7999/mcp",
    headers: {
      Authorization: "Bearer ${ZVEC_GREP_SERVER_TOKEN}",
    },
  });

  await uninstallTarget("cursor", {
    CURSOR_CONFIG_DIR: cursorConfigDirectory,
  });
  const uninstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(uninstalled.mcpServers, undefined);
});

test("OpenCode installer preserves config and manages a remote MCP server", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-opencode-"),
  );
  const configPath = join(temporaryDirectory, "opencode.json");
  const guidancePath = join(temporaryDirectory, "AGENTS.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await writeFile(guidancePath, "# Existing OpenCode guidance\n");
  await writeFile(
    configPath,
    `${JSON.stringify({ model: "custom/model", mcp: { other: { type: "remote", url: "https://example.com/mcp" } } }, null, 2)}\n`,
  );
  await installTarget("opencode", { OPENCODE_CONFIG: configPath }, [
    "--mcp-transport=http",
    "--mcp-tool-timeout=900",
    "--mcp-token-env=ZVEC_GREP_SERVER_TOKEN",
  ]);

  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(config.model, "custom/model");
  assert.equal(config.mcp.other.url, "https://example.com/mcp");
  assert.deepEqual(config.mcp.zvec_grep, {
    type: "remote",
    url: "http://127.0.0.1:7999/mcp",
    enabled: true,
    timeout: 900000,
    oauth: false,
    headers: {
      Authorization: "Bearer {env:ZVEC_GREP_SERVER_TOKEN}",
    },
  });
  const guidance = await readFile(guidancePath, "utf8");
  assert.match(guidance, /# Existing OpenCode guidance/);
  assert.match(
    guidance,
    /when an exact word, phrase, name, date,[^\n]+use `zvec_grep_zvec_grep_rg` when it is listed by the current host; otherwise native Grep or `rg`/i,
  );
  assert.match(
    guidance,
    /Use `zvec_grep_zvec_grep_search` when wording or location is unknown/,
  );
  assert.match(
    guidance,
    /Choose the evidence source before the retrieval mode/,
  );
  assert.match(guidance, /one focused `zvec_grep_zvec_grep_search` probe/);
  assert.match(guidance, /`zvec_grep_zvec_grep_rg` when it is listed/);
  assert.match(guidance, /probe does not apply to exact quotations/i);
  assert.match(
    guidance,
    /unrelated open-world questions, current external facts/,
  );
  assert.doesNotMatch(guidance, /managed-rg/);
  assert.equal(countOccurrences(guidance, "<!-- ZVEC_GREP_START -->"), 1);
  assert.equal(countOccurrences(guidance, "<!-- ZVEC_GREP_END -->"), 1);

  await uninstallTarget("opencode", { OPENCODE_CONFIG: configPath });
  const uninstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(uninstalled.mcp.zvec_grep, undefined);
  assert.equal(uninstalled.mcp.other.url, "https://example.com/mcp");
  const uninstalledGuidance = await readFile(guidancePath, "utf8");
  assert.match(uninstalledGuidance, /# Existing OpenCode guidance/);
  assert.doesNotMatch(uninstalledGuidance, /ZVEC_GREP|## zvec-grep/);
});

test("JSON installers require force before replacing an unmanaged server", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-json-conflict-"),
  );
  const configPath = join(temporaryDirectory, "opencode.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });
  await writeFile(
    configPath,
    '{"mcp":{"zvec_grep":{"type":"remote","url":"https://example.com/mcp"}}}\n',
  );

  await assert.rejects(
    installTarget("opencode", { OPENCODE_CONFIG: configPath }),
    /--force/,
  );
  await installTarget("opencode", { OPENCODE_CONFIG: configPath }, [
    "--force",
    "--mcp-transport=http",
  ]);
  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(config.mcp.zvec_grep.url, "http://127.0.0.1:7999/mcp");
});

test(
  "auto target installs only detected agents",
  {
    skip:
      process.platform === "win32" ? "PATH executable semantics differ" : false,
  },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-install-auto-"),
    );
    const binaryDirectory = join(temporaryDirectory, "bin");
    const claudeConfigDirectory = join(temporaryDirectory, ".claude");
    const codexHome = join(temporaryDirectory, ".codex");
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    await mkdir(binaryDirectory, { recursive: true });
    const claudeExecutable = join(binaryDirectory, "claude");
    await writeFile(claudeExecutable, "#!/bin/sh\n");
    await chmod(claudeExecutable, 0o755);

    const { stdout } = await execFileAsync(
      process.execPath,
      [cliPath, "install", "--yes"],
      {
        env: {
          ...process.env,
          PATH: binaryDirectory,
          HOME: temporaryDirectory,
          CLAUDE_CONFIG_DIR: claudeConfigDirectory,
          CODEX_HOME: codexHome,
          ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
        },
      },
    );

    assert.match(stdout, /Claude Code/);
    assert.doesNotMatch(stdout, /Codex/);
    await stat(join(claudeConfigDirectory, ".claude.json"));
    await assert.rejects(stat(join(codexHome, "config.toml")), {
      code: "ENOENT",
    });
  },
);

test(
  "auto target installs Qwen Code when only qwen is detected",
  {
    skip:
      process.platform === "win32" ? "PATH executable semantics differ" : false,
  },
  async (t) => {
    const temporaryDirectory = await mkdtemp(
      join(tmpdir(), "zvec-grep-install-auto-qwen-"),
    );
    const binaryDirectory = join(temporaryDirectory, "bin");
    const qwenHome = join(temporaryDirectory, ".qwen");
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    await mkdir(binaryDirectory, { recursive: true });
    const qwenExecutable = join(binaryDirectory, "qwen");
    await writeFile(qwenExecutable, "#!/bin/sh\n");
    await chmod(qwenExecutable, 0o755);

    const { stdout } = await execFileAsync(
      process.execPath,
      [cliPath, "install", "--yes"],
      {
        env: {
          ...process.env,
          PATH: binaryDirectory,
          HOME: temporaryDirectory,
          USERPROFILE: temporaryDirectory,
          QWEN_HOME: qwenHome,
          ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
        },
      },
    );

    assert.match(stdout, /Qwen Code/);
    assert.doesNotMatch(stdout, /Claude Code|Codex|OpenCode|Cursor/);
    const config = JSON.parse(
      await readFile(join(qwenHome, "settings.json"), "utf8"),
    );
    assert.equal(config.mcpServers.zvec_grep.command, "zg");
  },
);

async function installCodex(codexHome, extraArgs = []) {
  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "codex", "--yes", ...extraArgs],
    {
      env: {
        ...process.env,
        CODEX_HOME: codexHome,
        ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
      },
    },
  );
}

async function uninstallCodex(codexHome, extraArgs = []) {
  await execFileAsync(
    process.execPath,
    [cliPath, "uninstall", "--target", "codex", "--yes", ...extraArgs],
    {
      env: {
        ...process.env,
        CODEX_HOME: codexHome,
      },
    },
  );
}

async function installTarget(target, env, extraArgs = []) {
  const environment = {
    ...process.env,
    ZVEC_GREP_INSTALL_SKIP_SERVER: "1",
    ...env,
  };
  for (const [key, value] of Object.entries(environment)) {
    if (value === undefined) delete environment[key];
  }
  if (isQoderTarget(target) && !environment.QODER_PROJECT_DIR) {
    throw new Error("Qoder tests must set a temporary QODER_PROJECT_DIR");
  }
  return execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", target, "--yes", ...extraArgs],
    { env: environment },
  );
}

async function uninstallTarget(target, env, extraArgs = []) {
  if (isQoderTarget(target) && !env.QODER_PROJECT_DIR) {
    throw new Error("Qoder tests must set a temporary QODER_PROJECT_DIR");
  }
  await execFileAsync(
    process.execPath,
    [cliPath, "uninstall", "--target", target, "--yes", ...extraArgs],
    { env: { ...process.env, ...env } },
  );
}

function countOccurrences(value, search) {
  return value.split(search).length - 1;
}

function isQoderTarget(target) {
  return ["qoder", "qodercli", "qoder-cli", "6"].includes(String(target));
}

function stableStdioArgs(extraArgs = []) {
  return [cliPath, "server", "--stdio", ...extraArgs];
}

function assertCodexStableStdioLaunch(config, extraArgs = []) {
  assert.ok(config.includes(`command = ${JSON.stringify(process.execPath)}\n`));
  assert.ok(
    config.includes(
      `args = [${stableStdioArgs(extraArgs)
        .map((value) => JSON.stringify(value))
        .join(", ")}]\n`,
    ),
  );
}

function parseCodexStdioLaunch(config) {
  const match = config.match(
    /\[mcp_servers\.zvec_grep\]\ncommand = ("[^\n]+")\nargs = (\[[^\n]+\])\n/,
  );
  assert.ok(match, "Codex zvec_grep stdio configuration is missing");
  return {
    command: JSON.parse(match[1]),
    args: JSON.parse(match[2]),
  };
}

async function availablePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  await new Promise((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
  return address.port;
}
