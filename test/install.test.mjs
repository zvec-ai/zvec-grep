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
      },
    },
  );

  const installed = await readFile(configPath, "utf8");
  const agents = await readFile(agentsPath, "utf8");
  assert.match(installed, /\[mcp_servers\.other\]/);
  assert.doesNotMatch(
    installed,
    /mcp_servers\.zvec_grep\.tools\.zvec_grep_index/,
  );
  assert.doesNotMatch(agents, /zvec_grep_(?:index|status)/);
  assert.match(agents, /zg --index/);
  assert.match(agents, /zg --status/);
  assert.match(installed, /^tool_timeout_sec = 600$/m);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_START"), 1);
  assert.equal(countOccurrences(installed, "# ZVEC_GREP_END"), 1);
  assert.equal(countOccurrences(installed, "[mcp_servers.zvec_grep]"), 1);
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

function countOccurrences(value, search) {
  return value.split(search).length - 1;
}
