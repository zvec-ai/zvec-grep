import assert from "node:assert/strict";
import { mkdir, readFile, rename, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import test from "node:test";
import {
  cliPath,
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";
import { createFakeEmbeddingServer } from "../helpers/fake-embedding.mjs";
import { ensureSearchServer } from "../../dist/client/ensure-server.js";
import { DaemonClient } from "../../dist/client/daemon-client.js";
import { daemonTokenPath } from "../../dist/daemon/config.js";
import {
  readInstanceRecord,
  stopServer,
} from "../../dist/daemon/server-controller.js";

async function availablePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

test("custom endpoint fallback does not report an exhaustive semantic no-match", async (t) => {
  const temporary = await createTemporaryDirectory(
    t,
    "zvec-unavailable-semantic-",
  );
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(root);
  await writeFile(
    join(root, "auth.ts"),
    "export function verifyToken(token) { return Boolean(token); }\n",
  );
  const result = await runCli(["where does permission validation occur"], {
    cwd: root,
    env: {
      HOME: home,
      USERPROFILE: home,
      ZVEC_GREP_HOME: home,
      NO_COLOR: "1",
      ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/custom-endpoint`,
    },
    timeout: 5_000,
  });
  assert.equal(
    result.stdout,
    "No text matches; semantic search is not ready.\n",
  );
  assert.match(result.stderr, /text search only/);
  assert.match(result.stderr, /semantic index is not ready/);
  assert.equal(await readInstanceRecord(home), undefined);
  await assert.rejects(stat(join(root, ".zvec-grep")), { code: "ENOENT" });
});

test("a cold exact-symbol lookup searches current files without index, model or daemon", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-live-default-");
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(root);
  await writeFile(
    join(root, "auth.ts"),
    "export function verifyRemotePermission() { return true; }\n",
  );
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
  };
  const startedAt = performance.now();
  const first = await runCli(["verifyRemotePermission"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.match(first.stdout, /auth\.ts/);
  assert.match(first.stdout, /verifyRemotePermission/);
  assert.doesNotMatch(first.stderr, /index|model|Preparing|Downloading/);
  assert.equal(await readInstanceRecord(home), undefined);
  await assert.rejects(stat(join(root, ".zvec-grep")), {
    code: "ENOENT",
  });
  t.diagnostic(
    `cold exact-symbol search: ${Math.round(performance.now() - startedAt)} ms`,
  );
  await writeFile(
    join(root, "auth.ts"),
    "// inserted line\nexport function verifyRemotePermission() { return false; }\n",
  );
  const changed = await runCli(["verifyRemotePermission"], { cwd: root, env });
  assert.match(changed.stdout, /return false/);
  assert.doesNotMatch(changed.stdout, /return true/);
});

test("cold semantic preparation returns promptly even when model downloads fail", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-cold-offline-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(root);
  await writeFile(
    join(root, "auth.ts"),
    'export const message = "remote authorization";\n',
  );
  const preload = new URL(
    "../helpers/failing-model-download.mjs",
    import.meta.url,
  );
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_MODEL_CACHE: join(temporary, "models"),
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/mcp`,
    NODE_OPTIONS: `--import=${pathToFileURL(preload.pathname).href}`,
  };
  t.after(async () => {
    await runCli(["--server", "off"], { cwd: root, env }).catch(
      () => undefined,
    );
    await removeTemporaryDirectory(temporary);
  });
  const startedAt = performance.now();
  const first = await runCli(["remote", "authorization"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.match(first.stdout, /remote authorization/);
  assert.match(first.stderr, /text search only.*background/);
  assert.doesNotMatch(
    `${first.stdout}\n${first.stderr}`,
    /model-download-secret/,
  );
  assert.ok(
    (await readInstanceRecord(home))?.ready,
    "background process survives CLI exit",
  );
  t.diagnostic(
    `cold offline text result + background preparation: ${Math.round(performance.now() - startedAt)} ms`,
  );
});

test("cold property-assigned function lookup ranks the actual definition before examples without preparing an index", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-cold-bindings-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(root);
  const path = join(root, "response.js");
  await writeFile(
    path,
    [
      "// Example: res.sendFile(path)",
      'const example = "sendFile(path)";',
      "/** Transfer a response file. */",
      "res.sendFile = function implementation(path) {",
      "  return streamFile(path);",
      "};",
      "res.sendFile(example);",
    ].join("\n"),
  );
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_MODEL_CACHE: join(temporary, "models"),
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/mcp`,
  };
  t.after(async () => {
    await runCli(["--server", "off"], { cwd: root, env }).catch(
      () => undefined,
    );
    await removeTemporaryDirectory(temporary);
  });
  const start = performance.now();
  const result = await runCli(["sendFile"], { cwd: root, env, timeout: 5_000 });
  const firstMatch = result.stdout
    .split("\n")
    .find((line) => /^ {2}\d+(?:-\d+ \[|:)/.test(line));
  assert.match(firstMatch ?? "", /^ {2}4(?:-6 \[function res\.sendFile\]|:)/);
  assert.match(result.stdout, /res\.sendFile = function implementation/);
  assert.equal(result.stderr, "");
  assert.equal(await readInstanceRecord(home), undefined);
  await assert.rejects(stat(join(root, ".zvec-grep")), { code: "ENOENT" });
  await assert.rejects(stat(env.ZVEC_GREP_MODEL_CACHE), { code: "ENOENT" });
  t.diagnostic(
    `cold assigned-function lookup: ${Math.round(performance.now() - start)} ms`,
  );
  await writeFile(path, 'res.sendFile = (path) => "updated " + path;\n');
  const updated = await runCli(["sendFile"], { cwd: root, env });
  assert.match(updated.stdout, /updated/);
  assert.doesNotMatch(updated.stdout, /implementation/);
  assert.equal(await readInstanceRecord(home), undefined);
});

test("default file lookups and absent identifiers stay current without starting semantic search", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-default-files-");
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "auth.ts"),
    "export const enabled = true;\n",
  );
  await writeFile(
    join(root, "references.ts"),
    'const doc = "src/missing.ts";\n',
  );
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
  };
  const exact = await runCli(["src/auth.ts"], { cwd: root, env });
  assert.equal(exact.stdout, "#1 matchedBy=path src/auth.ts\n");
  assert.equal(exact.stderr, "");
  const basename = await runCli(["auth.ts"], { cwd: root, env });
  assert.equal(basename.stdout, exact.stdout);
  for (const query of [
    "nonexistentZebraPaymentHandler",
    "src/no-such-file.ts",
  ]) {
    const absent = await runCli([query], { cwd: root, env });
    assert.equal(absent.stdout, "No matches.\n");
    assert.equal(absent.stderr, "");
  }
  const reference = await runCli(["src/missing.ts"], { cwd: root, env });
  assert.match(reference.stdout, /references\.ts/);
  const excluded = await runCli(["auth.ts", "-g", "references.ts"], {
    cwd: root,
    env,
  });
  assert.equal(excluded.stdout, "No matches.\n");
  await rename(join(root, "src", "auth.ts"), join(root, "src", "new.ts"));
  const removed = await runCli(["src/auth.ts"], { cwd: root, env });
  assert.equal(removed.stdout, "No matches.\n");
  const added = await runCli(["src/new.ts"], { cwd: root, env });
  assert.equal(added.stdout, "#1 matchedBy=path src/new.ts\n");
  assert.equal(await readInstanceRecord(home), undefined);
  await assert.rejects(stat(join(root, ".zvec-grep")), { code: "ENOENT" });
});

test("a failed first semantic preparation is distinguished from an exhaustive no-match answer", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-empty-offline-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(root);
  await writeFile(
    join(root, "auth.ts"),
    "export function verifyToken(token) { return Boolean(token); }\n",
  );
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_MODEL_CACHE: join(temporary, "models"),
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/mcp`,
    NODE_OPTIONS: `--import=${new URL("../helpers/failing-model-download.mjs", import.meta.url).href}`,
  };
  t.after(async () => {
    await runCli(["--server", "off"], { cwd: root, env });
    await removeTemporaryDirectory(temporary);
  });
  const first = await runCli(["where does permission validation occur"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.equal(
    first.stdout,
    "No text matches; semantic search is not ready.\n",
  );
  assert.match(first.stderr, /semantic index preparation failed/);
  assert.doesNotMatch(first.stderr, /model-download-secret|cancelled by user/);
  const retry = await runCli(["where does permission validation occur"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.equal(retry.stdout, first.stdout);
  assert.match(retry.stderr, /semantic index preparation failed/);
  assert.doesNotMatch(retry.stderr, /model-download-secret|cancelled by user/);
});

test("cold keyword-positive search returns before held preparation and reuses its background job", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-slow-initial-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  const gate = join(temporary, "release-download");
  await mkdir(root);
  await writeFile(
    join(root, "auth.ts"),
    "// permission validation\nexport function verifyToken(token) { return Boolean(token); }\n",
  );
  const serverUrl = `http://127.0.0.1:${await availablePort()}/mcp`;
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_MODEL_CACHE: join(temporary, "models"),
    ZVEC_GREP_SERVER_URL: serverUrl,
    ZVEC_TEST_DOWNLOAD_GATE: gate,
    NODE_OPTIONS: `--import=${new URL("../helpers/gated-model-download.mjs", import.meta.url).href}`,
  };
  t.after(async () => {
    await writeFile(gate, "release");
    await runCli(["--server", "off"], { cwd: root, env });
    await removeTemporaryDirectory(temporary);
  });
  const started = performance.now();
  const first = await runCli(["where does permission validation occur"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.equal(
    first.stdout,
    "auth.ts\n  matchedBy=keyword\n  1:\t// permission validation\n",
  );
  assert.match(
    first.stderr,
    /semantic.*not ready|semantic readiness is unknown/i,
  );
  const firstDuration = performance.now() - started;
  assert.ok(
    firstDuration < 5_000,
    "useful keyword evidence should not wait for the 8-second index-readiness budget",
  );
  await waitForDownloadStart(gate);
  await assert.rejects(stat(gate), { code: "ENOENT" });
  const daemon = await readInstanceRecord(home);
  assert.ok(daemon?.ready, "the daemon survives the early CLI return");
  const client = new DaemonClient({ serverUrl, home });
  const before = await client.callTool("zvec_grep_index_status", { root });
  assert.equal(before.runtime.job_state, "running");
  const text = await runCli(["where does permission validation occur"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.equal(text.stdout, first.stdout);
  assert.match(text.stderr, /text search only/);
  await writeFile(
    join(root, "auth.ts"),
    "// permission validation CURRENT_DURING_PREPARATION\nexport function verifyToken(token) { return false; }\n",
  );
  await writeFile(
    join(root, "added.ts"),
    "// permission validation ADDED_DURING_PREPARATION\nexport const enabled = true;\n",
  );
  const changed = await runCli(["where does permission validation occur"], {
    cwd: root,
    env,
    timeout: 5_000,
  });
  assert.match(changed.stdout, /matchedBy=keyword/);
  assert.match(changed.stdout, /auth\.ts/);
  assert.match(changed.stdout, /CURRENT_DURING_PREPARATION/);
  assert.match(changed.stdout, /added\.ts/);
  assert.match(changed.stdout, /ADDED_DURING_PREPARATION/);
  assert.match(changed.stderr, /text search only/);
  const after = await client.callTool("zvec_grep_index_status", { root });
  assert.equal(after.runtime.active_job_id, before.runtime.active_job_id);
  assert.equal(after.runtime.job_state, "running");
  assert.equal((await readInstanceRecord(home)).pid, daemon.pid);
  await assert.rejects(stat(gate), { code: "ENOENT" });
  t.diagnostic(`cold keyword-positive return: ${Math.round(firstDuration)} ms`);
});

test("cold keyword-empty search retains its bounded semantic preparation opportunity", async (t) => {
  const rig = await gatedColdFixture(
    t,
    "zvec-keyword-empty-initial-",
    "export function verifyToken(token) { return Boolean(token); }\n",
  );
  const started = performance.now();
  const result = await rig.run(
    ["where does permission validation occur"],
    12_000,
  );
  const duration = performance.now() - started;
  assert.equal(
    result.stdout,
    "No text matches; semantic search is not ready.\n",
  );
  assert.match(result.stderr, /text search only/);
  assert.match(
    result.stderr,
    /semantic.*not ready|semantic readiness is unknown/i,
  );
  assert.ok(
    duration >= 7_500,
    "an empty keyword scan must not remove the initial semantic preparation opportunity",
  );
  assert.ok(duration < 11_000, "the initial readiness wait remains bounded");
  await waitForDownloadStart(rig.gate);
  await assert.rejects(stat(rig.gate), { code: "ENOENT" });
  const status = await rig.client.callTool("zvec_grep_index_status", {
    root: rig.root,
  });
  assert.equal(status.runtime.job_state, "running");
});

for (const [label, argumentsForQuery] of [
  [
    "refresh wait",
    ["--refresh", "wait", "where does permission validation occur"],
  ],
  ["hybrid", ["--hybrid", "where does permission validation occur"]],
]) {
  test(`cold explicit ${label} does not return keyword success while semantic preparation is held`, async (t) => {
    const rig = await gatedColdFixture(
      t,
      `zvec-explicit-${label.replaceAll(" ", "-")}-initial-`,
      "// permission validation\nexport function verifyToken(token) { return Boolean(token); }\n",
    );
    let settled = false;
    const outcome = rig.run(argumentsForQuery, 15_000).then(
      (value) => {
        settled = true;
        return { kind: "success", value };
      },
      (error) => {
        settled = true;
        return { kind: "failure", error };
      },
    );
    await waitForDownloadStart(rig.gate);
    // Give an accidental ordinary-keyword shortcut time to finish after the
    // job has entered model preparation. Only this test releases the gate.
    await delay(500);
    assert.equal(
      settled,
      false,
      "explicit search returned before model preparation settled",
    );
    await assert.rejects(stat(rig.gate), { code: "ENOENT" });
    const status = await rig.client.callTool("zvec_grep_index_status", {
      root: rig.root,
    });
    assert.equal(status.runtime.job_state, "running");
    await writeFile(rig.gate, "release");
    const completed = await outcome;
    assert.equal(
      completed.kind,
      "failure",
      "explicit semantic failure must not become a keyword-only success",
    );
    assert.equal(completed.error.killed, false);
    assert.match(completed.error.stderr, /Failed to download model/);
    assert.doesNotMatch(
      completed.error.stdout,
      /matchedBy=keyword|No text matches/,
    );
  });
}

async function gatedColdFixture(t, prefix, source) {
  const temporary = await createTemporaryDirectory(t, prefix, {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  const gate = join(temporary, "release-download");
  await mkdir(root);
  await writeFile(join(root, "auth.ts"), source);
  const serverUrl = `http://127.0.0.1:${await availablePort()}/mcp`;
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_MODEL_CACHE: join(temporary, "models"),
    ZVEC_GREP_SERVER_URL: serverUrl,
    ZVEC_TEST_DOWNLOAD_GATE: gate,
    NODE_OPTIONS: `--import=${new URL("../helpers/gated-model-download.mjs", import.meta.url).href}`,
  };
  const pending = [];
  t.after(async () => {
    await writeFile(gate, "release");
    await Promise.allSettled(pending);
    try {
      await runCli(["--server", "off"], { cwd: root, env, timeout: 10_000 });
    } finally {
      await removeTemporaryDirectory(temporary);
    }
  });
  return {
    root,
    gate,
    client: new DaemonClient({ serverUrl, home }),
    run: (args, timeout) => {
      const result = runCli(args, { cwd: root, env, timeout });
      pending.push(result);
      void result.catch(() => {});
      return result;
    },
  };
}

async function waitForDownloadStart(gate) {
  const deadline = performance.now() + 3_000;
  while (performance.now() < deadline) {
    try {
      await stat(`${gate}.started`);
      return;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    await delay(25);
  }
  assert.fail("background model preparation did not reach its download gate");
}

test("concurrent implicit startup reuses one authenticated daemon across clients", async (t) => {
  const home = await createTemporaryDirectory(t, "zvec-implicit-daemon-", {
    cleanup: false,
  });
  t.after(async () => {
    await stopServer(home);
    await removeTemporaryDirectory(home);
  });
  const serverUrl = `http://127.0.0.1:${await availablePort()}/mcp`;
  const options = { cliPath, home, serverUrl };
  const startedAt = performance.now();
  const urls = await Promise.all([
    ensureSearchServer(options),
    ensureSearchServer(options),
  ]);
  assert.deepEqual(urls, [serverUrl, serverUrl]);
  const first = await readInstanceRecord(home);
  assert.ok(first?.ready);
  assert.equal(await ensureSearchServer(options), serverUrl);
  assert.equal((await readInstanceRecord(home)).pid, first.pid);
  const response = await fetch(serverUrl);
  assert.equal(
    response.status,
    401,
    "implicit MCP endpoint must require authentication",
  );
  const token = (await readFile(daemonTokenPath(home), "utf8")).trim();
  assert.ok(token.length >= 32);
  const client = new DaemonClient({ serverUrl, home });
  const info = await client.callTool("zvec_grep_index_status", { root: home });
  assert.equal(info.indexed, false);
  t.diagnostic(
    `implicit start + reuse + authenticated request: ${Math.round(performance.now() - startedAt)} ms`,
  );
});

test("default auto queries implicitly start and reuse the daemon with one positional intent", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-default-search-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  await mkdir(root);
  await writeFile(
    join(root, "auth.ts"),
    'export function authorizeRemoteRequest() { return "remote authorization"; }\n',
  );
  await writeFile(
    join(root, "excluded.ts"),
    'export function authorizeRemoteRequest() { return "out-of-scope-value"; }\n',
  );
  const endpoint = await createFakeEmbeddingServer(t);
  const env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/mcp`,
  };
  t.after(async () => {
    await runCli(["--server", "off"], { cwd: root, env }).catch(
      () => undefined,
    );
    await removeTemporaryDirectory(temporary);
  });
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
      "auth.ts",
      root,
    ],
    { cwd: root, env },
  );
  assert.equal(await readInstanceRecord(home), undefined);
  const first = await runCli(["remote", "authorization", "--allow-remote"], {
    cwd: root,
    env,
  });
  assert.match(first.stdout, /auth\.ts/);
  assert.doesNotMatch(first.stdout, /query groups|Q2 \[primary\]/);
  assert.doesNotMatch(first.stderr, /server unavailable/);
  const daemon = await readInstanceRecord(home);
  assert.ok(daemon?.ready);
  const second = await runCli(["--fts", "authorizeRemoteRequest"], {
    cwd: root,
    env,
  });
  assert.match(second.stdout, /auth\.ts/);
  assert.equal((await readInstanceRecord(home)).pid, daemon.pid);
  const exact = await runCli(["authorizeRemoteRequest"], { cwd: root, env });
  assert.match(exact.stdout, /auth\.ts/);
  assert.doesNotMatch(exact.stdout, /\.\.[\\/]/);
  assert.doesNotMatch(exact.stdout, /excluded\.ts|out-of-scope-value/);
  const file = await runCli(["auth.ts"], { cwd: root, env });
  assert.equal(file.stdout, "#1 matchedBy=path auth.ts\n");
  for (const query of ["excluded.ts", "nonexistentZebraPaymentHandler"]) {
    const absent = await runCli([query], { cwd: root, env });
    assert.equal(absent.stdout, "No matches.\n");
  }
  const approximate = await runCli(
    ["--vector", "nonexistentZebraPaymentHandler", "--allow-remote"],
    { cwd: root, env },
  );
  assert.match(approximate.stdout, /auth\.ts/);
  const restricted = await runCli(
    ["authorizeRemoteRequest", "-g", "excluded.ts", "--allow-remote"],
    { cwd: root, env },
  );
  assert.doesNotMatch(restricted.stdout, /out-of-scope-value/);
});
