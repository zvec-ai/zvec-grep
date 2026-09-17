import assert from "node:assert/strict";
import { mkdir, rm, utimes, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import test from "node:test";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";
import { createFakeEmbeddingServer } from "../helpers/fake-embedding.mjs";

async function availablePort() {
  const server = createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

test("ordinary search combines current matches with index evidence and never displays superseded or deleted source", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-live-index-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  let env;
  t.after(async () => {
    if (env) {
      await runCli(["--server", "off"], { cwd: root, env }).catch(
        () => undefined,
      );
    }
    await removeTemporaryDirectory(temporary);
  });
  await mkdir(root);
  await writeFile(
    join(root, "changed.ts"),
    'export function openPool() { return "connection pool OLD_VERSION"; }\n',
  );
  await writeFile(
    join(root, "deleted.ts"),
    'export const deleted = "connection pool REMOVED_VERSION";\n',
  );
  await writeFile(
    join(root, "stable.ts"),
    'export function reuseConnections() { return "reuse database connections"; }\n',
  );
  await writeFile(
    join(root, "excluded.ts"),
    'export const excluded = "connection pool SHOULD_NOT_LEAK";\n',
  );
  const endpoint = await createFakeEmbeddingServer(t);
  env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/mcp`,
  };
  await runCli(
    [
      "--index",
      "--mode",
      "direct",
      "--embedding",
      "qwen/text-embedding-v4",
      "--api-key",
      "fixture-key",
      "--endpoint",
      endpoint,
      "--allow-remote",
      "-g",
      "{changed,deleted,stable,added}.ts",
      root,
    ],
    { cwd: root, env },
  );
  await writeFile(
    join(root, "changed.ts"),
    '// inserted line\nexport function openPool() { return "connection pool CURRENT_VERSION"; }\n',
  );
  // Deliberately advance mtime so the fixture does not rely on filesystem
  // timestamp granularity to distinguish the saved index from current files.
  const modified = new Date(Date.now() + 5_000);
  await utimes(join(root, "changed.ts"), modified, modified);
  await rm(join(root, "deleted.ts"));
  await writeFile(
    join(root, "added.ts"),
    'export function newPool() { return "connection pool NEW_FILE"; }\n',
  );
  const snapshot = await runCli(
    ["--fts", "connection pool", "--mode", "direct", "--refresh", "off"],
    { cwd: root, env },
  );
  assert.match(snapshot.stdout, /OLD_VERSION/);
  assert.match(snapshot.stdout, /REMOVED_VERSION/);
  const direct = await runCli(
    [
      "connection pool",
      "--mode",
      "direct",
      "--refresh",
      "off",
      "--allow-remote",
    ],
    { cwd: root, env },
  );
  assert.match(direct.stdout, /CURRENT_VERSION/);
  assert.match(direct.stdout, /NEW_FILE/);
  assert.doesNotMatch(
    direct.stdout,
    /OLD_VERSION|REMOVED_VERSION|SHOULD_NOT_LEAK/,
  );
  assert.match(direct.stderr, /omitted 2 outdated indexed results/);
  const incomplete = await runCli(
    [
      "where are connections reused",
      "-g",
      "{changed,deleted}.ts",
      "--mode",
      "direct",
      "--refresh",
      "off",
      "--allow-remote",
    ],
    { cwd: root, env },
  );
  assert.equal(
    incomplete.stdout,
    "No current matches; index results are incomplete.\n",
  );
  assert.match(incomplete.stderr, /omitted 2 outdated indexed results/);
  const current = await runCli(["connection pool", "--allow-remote"], {
    cwd: root,
    env,
  });
  assert.match(current.stdout, /CURRENT_VERSION/);
  assert.match(current.stdout, /NEW_FILE/);
  assert.match(current.stdout, /stable\.ts/);
  assert.doesNotMatch(
    current.stdout,
    /OLD_VERSION|REMOVED_VERSION|SHOULD_NOT_LEAK/,
  );
  assert.equal(
    (current.stdout.match(/#[0-9]+[^\n]*changed\.ts/g) ?? []).length,
    1,
  );
  const restricted = await runCli(
    ["connection pool", "-g", "changed.ts", "--limit", "1", "--allow-remote"],
    { cwd: root, env },
  );
  assert.match(restricted.stdout, /CURRENT_VERSION/);
  assert.doesNotMatch(
    restricted.stdout,
    /added\.ts|stable\.ts|SHOULD_NOT_LEAK/,
  );
  assert.equal((restricted.stdout.match(/^#[0-9]+ /gm) ?? []).length, 1);
});
