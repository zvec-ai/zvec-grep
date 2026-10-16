import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "./helpers/fixtures.mjs";
import { createFakeEmbeddingServer } from "./helpers/fake-embedding.mjs";

async function availablePort() {
  const { createServer } = await import("node:net");
  return new Promise((resolvePort) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolvePort(port));
    });
  });
}

async function prepareHeavyFixture(parent, name) {
  const root = join(parent, name, "repo");
  let dir = root;
  for (let depth = 0; depth < 8; depth++) {
    dir = join(dir, "a".repeat(200));
    await mkdir(dir, { recursive: true });
  }
  await writeFile(
    join(dir, `f${"g".repeat(201)}.ts`),
    "export const HealthProbeSymbol = 42;\n",
  );
  const rules = Array.from(
    { length: 100 },
    (_, i) => "*a".repeat(100) + "*Z" + i,
  );
  await writeFile(join(root, ".gitignore"), `${rules.join("\n")}\n`);
  return root;
}

/**
 * One probe over a run-owned daemon: overlapping index+query load while an
 * independent loop polls a health URL. Fails unless (a) every health request
 * succeeds with 200, (b) at least `minOkPolls` successful responses arrive
 * during load, (c) steady-state worst latency stays under `maxMs` (warmup
 * excluded), (d) indexing reports success and (e) the query finds the symbol.
 */
async function runHealthProbe(options) {
  const {
    root,
    home,
    env,
    port,
    healthUrl,
    minOkPolls = 50,
    maxMs = 400,
    expectWorkloadSuccess = true,
  } = options;
  await mkdir(join(home, ".zvec-grep"), { recursive: true });
  await writeFile(
    join(home, ".zvec-grep", "config.json"),
    `${JSON.stringify({
      version: 1,
      defaults: { embedding: "qwen/text-embedding-v4" },
    })}\n`,
  );
  const started = await runCli(
    ["--server", "on", "--listen", `127.0.0.1:${port}`, "--home", home],
    { cwd: root, env },
  );
  assert.match(started.stdout, /Server: ready/);

  let polling = true;
  let loadStartedAt = 0;
  let worstHealthMs = 0;
  let okPolls = 0;
  let loadOkPolls = 0;
  const failures = [];
  const healthUrl_ = healthUrl ?? `http://127.0.0.1:${port}/healthz`;
  const pollLoop = (async () => {
    while (polling) {
      const requestStart = Date.now();
      try {
        const response = await fetch(healthUrl_, {
          signal: AbortSignal.timeout(2_000),
        });
        const elapsed = Date.now() - requestStart;
        if (response.status !== 200) {
          failures.push(`status ${response.status}`);
        } else {
          okPolls++;
          if (loadStartedAt && Date.now() >= loadStartedAt) {
            loadOkPolls++;
            worstHealthMs = Math.max(worstHealthMs, elapsed);
          }
        }
      } catch (error) {
        failures.push(String(error?.cause?.code ?? error?.message ?? error));
      }
      await new Promise((resolveSleep) => setTimeout(resolveSleep, 10));
    }
  })();
  try {
    const warmupDeadline = Date.now() + 15_000;
    while (okPolls === 0 && Date.now() < warmupDeadline) {
      await new Promise((resolveSleep) => setTimeout(resolveSleep, 10));
    }
    assert.ok(okPolls > 0, "health endpoint never answered during warmup");

    // Expensive search filters: heavy globs evaluated against the fixture's
    // deep paths, alongside the matching pattern.
    const heavyGlobs = Array.from(
      { length: 100 },
      (_, i) => "*a".repeat(100) + "*Z" + i,
    );
    const queryArgs = [
      "--fts",
      "HealthProbeSymbol",
      "--limit",
      "1",
      "--refresh",
      "off",
      "--allow-remote",
      ...heavyGlobs.flatMap((glob) => ["--glob", glob]),
      "--glob",
      "*.ts",
    ];
    // Warmup completes only after the daemon has actually served workload on
    // the MEASURED target: a first index of the target repo opens its store
    // and prepares its model. Warmup failures propagate — a cold store must
    // never be measured as load.
    const warmupIndex = await runCli(
      ["--index", "--mode", "server", "--allow-remote", root],
      { cwd: root, env, timeout: 120_000 },
    );
    assert.match(
      String(warmupIndex.stdout),
      /Workspace index: succeeded/,
      "warmup index of the target store failed",
    );
    // Mutate the fixture input so the measured index performs real work
    // (rescan plus heavy-rule matching) against the already-warm store.
    await writeFile(
      join(root, "mutation.ts"),
      "export const HealthProbeMutation = 7;\n",
    );

    loadStartedAt = Date.now();
    const [indexed, queried] = await Promise.all([
      runCli(["--index", "--mode", "server", "--allow-remote", root], {
        cwd: root,
        env,
        timeout: 180_000,
      }).catch((error) => error),
      (async () => {
        await new Promise((resolveSleep) => setTimeout(resolveSleep, 200));
        return runCli([...queryArgs, "--mode", "server"], {
          cwd: root,
          env,
          timeout: 120_000,
        }).catch((error) => error);
      })(),
    ]);
    polling = false;
    await pollLoop;

    const problems = [];
    if (failures.length > 0) {
      problems.push(
        `${failures.length} health requests failed (${failures[0]})`,
      );
    }
    if (okPolls < minOkPolls) {
      problems.push(`only ${okPolls} successful health responses`);
    }
    if (loadOkPolls < 20) {
      problems.push(
        `only ${loadOkPolls} health responses during confirmed load`,
      );
    }
    if (worstHealthMs >= maxMs) {
      problems.push(`worst steady-state health latency ${worstHealthMs}ms`);
    }
    const indexedOk =
      !indexed?.code &&
      /Workspace index: succeeded/.test(String(indexed.stdout ?? ""));
    const queriedOk =
      queryResultProblems(queried, `f${"g".repeat(201)}.ts`).length === 0;
    if (expectWorkloadSuccess) {
      if (!indexedOk)
        problems.push(
          `indexing did not succeed: ${String(indexed?.stderr ?? indexed)}`.slice(
            0,
            200,
          ),
        );
      if (!queriedOk)
        problems.push(
          ...queryResultProblems(queried, `f${"g".repeat(201)}.ts`).map(
            (problem) => `query result: ${problem}`,
          ),
        );
    }
    return { problems, worstHealthMs, okPolls, loadOkPolls };
  } finally {
    polling = false;
    await pollLoop;
  }
}

/**
 * A query result counts only with successful execution, a positive parsed
 * hit count, and the expected file among the matched entries — echoing the
 * query text alone proves nothing.
 */
function queryResultProblems(queried, expectedFileName) {
  const problems = [];
  const stdout = String(queried?.stdout ?? "");
  if (queried?.code) {
    problems.push(
      `query failed: ${String(queried?.stderr ?? queried).slice(0, 120)}`,
    );
    return problems;
  }
  const match = /hits: (\d+)/.exec(stdout);
  const hitCount = match ? Number.parseInt(match[1], 10) : 0;
  if (!match) {
    problems.push("query output has no parseable hit count");
  } else if (hitCount < 1) {
    problems.push(`query returned ${hitCount} hits`);
  }
  if (
    hitCount >= 1 &&
    !new RegExp(`matchedBy=\\S+ .*${expectedFileName}`).test(stdout)
  ) {
    problems.push(
      `expected file ${expectedFileName} not among matched entries`,
    );
  }
  return problems;
}

async function readInstanceRecord(home) {
  // The daemon stores its record under <home>/daemon/instance.lock
  // (daemonHome in src/daemon/config.ts).
  const recordPath = join(home, "daemon", "instance.lock");
  const content = await readFile(recordPath, "utf8").catch(() => null);
  if (content === null) return null;
  try {
    return { recordPath, record: JSON.parse(content) };
  } catch {
    return { recordPath, record: null };
  }
}

function processAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/**
 * The single owner of cleanup, strictly ordered: identify the daemon from its
 * instance record before anything is removed, terminate it, confirm both
 * process exit and listener release, and only then allow directory removal.
 * If termination cannot be confirmed, the home and instance record stay in
 * place as ownership evidence and the failure is thrown.
 */
async function ownedTeardown({ home, root, env, port }) {
  const identified = await readInstanceRecord(home);
  const pid = identified?.record?.pid;
  await runCli(["--server", "off", "--home", home], { cwd: root, env }).catch(
    () => undefined,
  );
  if (typeof pid === "number" && pid > 0) {
    const deadline = Date.now() + 10_000;
    while (Date.now() < deadline && processAlive(pid)) {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  let listenerReleased = false;
  try {
    await fetch(`http://127.0.0.1:${port}/healthz`);
  } catch {
    listenerReleased = true;
  }
  const exited = !(typeof pid === "number" && pid > 0 && processAlive(pid));
  if (!exited || !listenerReleased) {
    throw new Error(
      `daemon teardown unconfirmed (pid ${pid}, exited=${exited}, listenerReleased=${listenerReleased}); home and instance record preserved at ${home}`,
    );
  }
}

test("daemon /healthz stays responsive under overlapping load", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-health-probe-",
    {
      cleanup: false,
    },
  );
  const root = await prepareHeavyFixture(temporaryDirectory, ".");
  const home = join(temporaryDirectory, "home");
  const endpoint = await createFakeEmbeddingServer(t);
  const port = await availablePort();
  const env = {
    HOME: home,
    USERPROFILE: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_ENDPOINT: endpoint,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
  };
  t.after(async () => {
    // Removal happens only after confirmed termination and listener release;
    // an unconfirmed teardown throws and the directories remain as evidence.
    await ownedTeardown({ home, root, env, port });
    await removeTemporaryDirectory(temporaryDirectory);
  });

  const { problems, worstHealthMs, okPolls } = await runHealthProbe({
    root,
    home,
    env,
    port,
  });
  assert.deepEqual(problems, []);
  assert.ok(okPolls >= 50, `expected sustained polling, got ${okPolls}`);
  assert.ok(
    worstHealthMs < 400,
    `worst steady-state latency ${worstHealthMs}ms`,
  );
  await ownedTeardown({ home, root, env, port });
  await removeTemporaryDirectory(temporaryDirectory);
});

test("health probe negative control: unavailable health fails the probe", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-health-dead-",
    {
      cleanup: false,
    },
  );
  const root = await prepareHeavyFixture(temporaryDirectory, ".");
  const home = join(temporaryDirectory, "home");
  const endpoint = await createFakeEmbeddingServer(t);
  const port = await availablePort();
  const deadPort = await availablePort();
  const env = {
    HOME: home,
    USERPROFILE: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_ENDPOINT: endpoint,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
  };
  t.after(async () => {
    // Removal happens only after confirmed termination and listener release;
    // an unconfirmed teardown throws and the directories remain as evidence.
    await ownedTeardown({ home, root, env, port });
    await removeTemporaryDirectory(temporaryDirectory);
  });
  // Polling a port with no listener: the probe must fail — either at warmup
  // (the endpoint never answers) or through reported request failures — even
  // though the daemon itself and its workload are healthy.
  let problems = null;
  let warmupThrew = null;
  try {
    ({ problems } = await runHealthProbe({
      root,
      home,
      env,
      port,
      healthUrl: `http://127.0.0.1:${deadPort}/healthz`,
    }));
  } catch (error) {
    warmupThrew = error;
  }
  if (warmupThrew) {
    assert.match(warmupThrew.message, /never answered during warmup/);
  } else {
    assert.ok(
      problems.length > 0,
      "probe unexpectedly passed with no health listener",
    );
    assert.ok(
      problems.some((p) => /health requests failed|successful health/.test(p)),
      JSON.stringify(problems),
    );
  }
  await ownedTeardown({ home, root, env, port });
  await removeTemporaryDirectory(temporaryDirectory);
});

test("health probe negative control: failed workload fails the probe", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-health-broken-",
    {
      cleanup: false,
    },
  );
  const root = await prepareHeavyFixture(temporaryDirectory, ".");
  const home = join(temporaryDirectory, "home");
  const deadEndpoint = `http://127.0.0.1:${await availablePort()}/v1`;
  const port = await availablePort();
  const env = {
    HOME: home,
    USERPROFILE: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_ENDPOINT: deadEndpoint,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
  };
  t.after(async () => {
    // Removal happens only after confirmed termination and listener release;
    // an unconfirmed teardown throws and the directories remain as evidence.
    await ownedTeardown({ home, root, env, port });
    await removeTemporaryDirectory(temporaryDirectory);
  });
  let problems = null;
  let warmupThrew = null;
  try {
    ({ problems } = await runHealthProbe({ root, home, env, port }));
  } catch (error) {
    warmupThrew = error;
  }
  if (warmupThrew) {
    // With a broken embedding endpoint the warmup index of the target store
    // fails and propagates — itself a detected workload failure.
    assert.match(
      String(warmupThrew.message ?? warmupThrew),
      /Command failed|warmup index .* failed|REQUEST_FAILED/,
    );
  } else {
    assert.ok(
      problems.length > 0,
      "probe unexpectedly passed with a failed workload",
    );
    assert.ok(
      /indexing did not succeed/.test(problems.join("; ")),
      JSON.stringify(problems),
    );
  }
  await ownedTeardown({ home, root, env, port });
  await removeTemporaryDirectory(temporaryDirectory);
});

test("teardown preserves ownership evidence when termination is unconfirmed", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-teardown-preserve-",
    {
      cleanup: false,
    },
  );
  const home = join(temporaryDirectory, "home");
  await mkdir(join(home, "daemon"), { recursive: true });
  const port = await availablePort();
  // A fabricated live daemon record: termination can never be confirmed, so
  // the teardown must throw and must NOT have removed the record or home.
  const fakePid = process.pid;
  await writeFile(
    join(home, "daemon", "instance.lock"),
    `${JSON.stringify({ pid: fakePid, serverUrl: `http://127.0.0.1:${port}` })}\n`,
  );
  let threw = null;
  try {
    await ownedTeardown({
      home,
      root: temporaryDirectory,
      env: process.env,
      port,
    });
  } catch (error) {
    threw = error;
  }
  assert.ok(threw, "teardown must throw when termination is unconfirmed");
  assert.match(threw.message, /teardown unconfirmed/);
  assert.match(threw.message, /preserved/);
  // Ownership evidence preserved: record and home still exist.
  const preserved = await readFile(
    join(home, "daemon", "instance.lock"),
    "utf8",
  );
  assert.match(preserved, new RegExp(`"pid":${fakePid}`));
});

test("teardown failure path preserves evidence with a live daemon", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-teardown-live-",
    { cleanup: false },
  );
  const root = temporaryDirectory;
  const home = join(temporaryDirectory, "home");
  await mkdir(join(home, "daemon"), { recursive: true });
  const port = await availablePort();
  const env = {
    ...process.env,
    ZVEC_GREP_HOME: home,
    HOME: home,
    NO_COLOR: "1",
  };
  const started = await runCli(
    ["--server", "on", "--listen", `127.0.0.1:${port}`, "--home", home],
    { cwd: root, env },
  );
  assert.match(started.stdout, /Server: ready/);
  const record = JSON.parse(
    await readFile(join(home, "daemon", "instance.lock"), "utf8"),
  );
  // Contained cleanup for the test itself: a correct stop that must succeed.
  t.after(async () => {
    await runCli(["--server", "off", "--home", home], { cwd: root, env }).catch(
      () => undefined,
    );
    try {
      process.kill(record.pid, "SIGKILL");
    } catch {
      // already gone
    }
    await removeTemporaryDirectory(temporaryDirectory);
  });
  // Sabotaged termination: the owning teardown is pointed at a home whose
  // record it cannot read and whose listener stays up, so termination is
  // unconfirmed; it must throw and preserve the real home's evidence.
  let threw = null;
  try {
    await ownedTeardown({
      home: join(temporaryDirectory, "elsewhere"),
      root,
      env,
      port,
    });
  } catch (error) {
    threw = error;
  }
  assert.ok(threw, "teardown must throw when termination is unconfirmed");
  assert.match(threw.message, /teardown unconfirmed/);
  let alive;
  try {
    process.kill(record.pid, 0);
    alive = true;
  } catch {
    alive = false;
  }
  assert.ok(alive, "daemon should still be alive in the failure path");
  const preserved = await readFile(
    join(home, "daemon", "instance.lock"),
    "utf8",
  );
  assert.match(preserved, new RegExp(`"pid":${record.pid}`));
});

test("query result assertion rejects zero-hit and wrong-file outputs", async () => {
  const expected = `f${"g".repeat(201)}.ts`;
  const zeroHit = {
    code: 0,
    stdout:
      "query groups (1):\nQ1 [supplemental]: HealthProbeSymbol\nhits: 0\n",
  };
  let threw;
  try {
    const problems = queryResultProblems(zeroHit, expected);
    if (problems.length === 0)
      throw new Error("zero-hit output unexpectedly accepted");
    threw = problems;
  } catch (error) {
    threw = [String(error.message)];
  }
  assert.ok(
    threw.some((p) => /returned 0 hits/.test(p)),
    "expected the zero-hit failure at the hit-count assertion",
  );
  const wrongFile = {
    code: 0,
    stdout:
      "query groups (1):\nhits: 1\n#1 matchedBy=fts src/other.ts:1-2\nexport const HealthProbeSymbol = 42;\n",
  };
  const wrongFileProblems = queryResultProblems(wrongFile, expected);
  assert.ok(
    wrongFileProblems.some((p) => /not among matched entries/.test(p)),
    "expected the wrong-file failure at the file assertion",
  );
  const good = {
    code: 0,
    stdout: `query groups (1):\nhits: 1\n#1 matchedBy=fts some/dir/${expected}:1-2\nexport const HealthProbeSymbol = 42;\n`,
  };
  assert.deepEqual(queryResultProblems(good, expected), []);
});
