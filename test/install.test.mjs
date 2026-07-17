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

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");

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
      },
    },
  );

  const installed = await readFile(configPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]/);
  assert.match(installed, /^url = "http:\/\/127\.0\.0\.1:7999\/mcp"$/m);
  assert.doesNotMatch(installed, /^bearer_token_env_var\s*=/m);
  assert.doesNotMatch(installed, /^command\s*=\s*"zg"$/m);
  assert.match(installed, /^tool_timeout_sec = 600$/m);
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
    { env: { ...process.env, CODEX_HOME: codexHome } },
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

test("Codex installer removes legacy managed guidance", async (t) => {
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
  assert.doesNotMatch(agents, /ZVEC_GREP|legacy guidance/);
});

test("Claude Code installer manages a user-scoped HTTP MCP server", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-claude-"),
  );
  const claudeConfigDirectory = join(temporaryDirectory, ".claude");
  const configPath = join(claudeConfigDirectory, ".claude.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await installTarget(
    "claude",
    {
      CLAUDE_CONFIG_DIR: claudeConfigDirectory,
    },
    ["--mcp-token-env", "ZVEC_GREP_SERVER_TOKEN"],
  );

  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.deepEqual(config.mcpServers.zvec_grep, {
    type: "http",
    url: "http://127.0.0.1:7999/mcp",
    headers: {
      Authorization: "Bearer ${ZVEC_GREP_SERVER_TOKEN}",
    },
  });

  await uninstallTarget("claude", {
    CLAUDE_CONFIG_DIR: claudeConfigDirectory,
  });
  const uninstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(uninstalled.mcpServers, undefined);
});

test("Claude Code installer accepts cc and claude-code compatibility aliases", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-install-claude-aliases-"),
  );
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  for (const alias of ["cc", "claude-code"]) {
    const claudeConfigDirectory = join(temporaryDirectory, alias);
    await installTarget(alias, { CLAUDE_CONFIG_DIR: claudeConfigDirectory });
    const config = JSON.parse(
      await readFile(join(claudeConfigDirectory, ".claude.json"), "utf8"),
    );
    assert.equal(config.mcpServers.zvec_grep.url, "http://127.0.0.1:7999/mcp");
  }
});

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
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await writeFile(
    configPath,
    `${JSON.stringify({ model: "custom/model", mcp: { other: { type: "remote", url: "https://example.com/mcp" } } }, null, 2)}\n`,
  );
  await installTarget("opencode", { OPENCODE_CONFIG: configPath }, [
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

  await uninstallTarget("opencode", { OPENCODE_CONFIG: configPath });
  const uninstalled = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(uninstalled.mcp.zvec_grep, undefined);
  assert.equal(uninstalled.mcp.other.url, "https://example.com/mcp");
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
  await installTarget("opencode", { OPENCODE_CONFIG: configPath }, ["--force"]);
  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(config.mcp.zvec_grep.url, "http://127.0.0.1:7999/mcp");
});

async function installCodex(codexHome, extraArgs = []) {
  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", "codex", "--yes", ...extraArgs],
    {
      env: {
        ...process.env,
        CODEX_HOME: codexHome,
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
  await execFileAsync(
    process.execPath,
    [cliPath, "install", "--target", target, "--yes", ...extraArgs],
    { env: { ...process.env, ...env } },
  );
}

async function uninstallTarget(target, env, extraArgs = []) {
  await execFileAsync(
    process.execPath,
    [cliPath, "uninstall", "--target", target, "--yes", ...extraArgs],
    { env: { ...process.env, ...env } },
  );
}

function countOccurrences(value, search) {
  return value.split(search).length - 1;
}
