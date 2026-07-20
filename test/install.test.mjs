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
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { installerSelectionLines } from "../dist/cli/commands.js";

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");

test("interactive installer marker follows the active agent", () => {
  const detected = new Set(["claude", "codex"]);
  const claude = installerSelectionLines(0, detected);
  const codex = installerSelectionLines(1, detected);

  assert.match(claude[0], /● Claude Code\s+detected/);
  assert.match(claude[1], /○ Codex\s+detected/);
  assert.match(codex[0], /○ Claude Code\s+detected/);
  assert.match(codex[1], /● Codex\s+detected/);
  assert.match(codex.at(-1), /Use ↑↓ to move · Enter to select/);
  assert.doesNotMatch(codex.join("\n"), /Space|\[●\]/);
});

test("Codex installer removes orphaned managed markers", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const configPath = join(codexHome, "config.toml");
  const agentsPath = join(codexHome, "AGENTS.md");
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
  const agents = await readFile(agentsPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]/);
  assert.match(installed, /^url = "http:\/\/127\.0\.0\.1:7999\/mcp"$/m);
  assert.doesNotMatch(installed, /^bearer_token_env_var\s*=/m);
  assert.doesNotMatch(installed, /^command\s*=\s*"zg"$/m);
  assert.match(agents, /zvec_grep_index/);
  assert.match(agents, /zg server on/);
  assert.match(agents, /`wait` defaults to false/i);
  assert.match(agents, /zvec_grep_index_status/);
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
    const agentsTarget = join(dotfiles, "AGENTS.md");
    const configPath = join(codexHome, "config.toml");
    const agentsPath = join(codexHome, "AGENTS.md");
    t.after(async () => {
      await rm(temporaryDirectory, { recursive: true, force: true });
    });

    await mkdir(codexHome, { recursive: true });
    await mkdir(dotfiles, { recursive: true });
    await writeFile(configTarget, '[mcp_servers.other]\ncommand = "other"\n');
    await writeFile(agentsTarget, "# Existing instructions\n");
    await chmod(configTarget, 0o640);
    await chmod(agentsTarget, 0o600);
    await symlink(configTarget, configPath);
    await symlink(agentsTarget, agentsPath);

    await installCodex(codexHome);

    assert.equal((await lstat(configPath)).isSymbolicLink(), true);
    assert.equal((await lstat(agentsPath)).isSymbolicLink(), true);
    assert.equal((await stat(configTarget)).mode & 0o777, 0o640);
    assert.equal((await stat(agentsTarget)).mode & 0o777, 0o600);
    assert.match(
      await readFile(configTarget, "utf8"),
      /\[mcp_servers\.zvec_grep\]/,
    );
    assert.match(await readFile(agentsTarget, "utf8"), /## zvec-grep/);
  },
);

test("Codex installer removes temporary files when an atomic replacement fails", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-failure-"),
  );
  const codexHome = join(temporaryDirectory, ".codex");
  const agentsPath = join(codexHome, "AGENTS.md");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await mkdir(agentsPath, { recursive: true });

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
    type: "http",
    url: "http://127.0.0.1:7999/mcp",
    alwaysLoad: true,
  });
  assert.ok(settings.permissions.allow.includes("mcp__zvec_grep__*"));
  assert.match(guidance, /Remote data authorization/);
  assert.match(stdout, /zvec-grep setup/);
  assert.match(stdout, /Installing integrations/);
  assert.match(stdout, /Claude Code/);
  assert.doesNotMatch(stdout, /Guidance/);
  assert.match(stdout, /MCP trust\s+Approved/);
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

function countOccurrences(value, search) {
  return value.split(search).length - 1;
}
