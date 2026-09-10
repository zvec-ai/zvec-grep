import assert from "node:assert/strict";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createServer as createTcpServer } from "node:net";
import { join } from "node:path";
import test from "node:test";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";
import { deterministicVector } from "../helpers/fake-embedding.mjs";

const query = "connection pool";
const missingQuery = "unfindable semantic concept";
const fallbackWarning =
  "warning: semantic search unavailable; showing current text matches only (search coverage is incomplete)";

async function availablePort() {
  const server = createTcpServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function fixture(t, mode, { race = false, keyword = false } = {}) {
  const temporary = await createTemporaryDirectory(
    t,
    "zvec-default-query-fallback-",
    { cleanup: false },
  );
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  let env;
  let rejectQueryEmbedding = false;
  let failureStatus = 503;
  let documentRequests = 0;
  let failedQueryRequests = 0;
  let beforeQueryFailure;
  const server = createServer(async (request, response) => {
    try {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      const inputs = Array.isArray(body.input) ? body.input : [];
      if (
        rejectQueryEmbedding &&
        inputs.length === 1 &&
        [query, missingQuery].includes(inputs[0])
      ) {
        failedQueryRequests++;
        await beforeQueryFailure?.();
        response.writeHead(failureStatus, {
          "content-type": "application/json",
          "retry-after": "0",
        });
        response.end(
          JSON.stringify({
            error: {
              code:
                failureStatus === 401
                  ? "fixture_invalid_api_key"
                  : "fixture_query_embedding_unavailable",
              message: "fixture_private_provider_detail",
              type:
                failureStatus === 401
                  ? "authentication_error"
                  : "service_unavailable_error",
            },
          }),
        );
        return;
      }
      documentRequests++;
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          data: inputs.map((input, index) => ({
            index,
            embedding: deterministicVector(input),
          })),
        }),
      );
    } catch (error) {
      response.destroy(error);
    }
  });
  t.after(async () => {
    try {
      if (env && mode === "auto") {
        await runCli(["--server", "off"], {
          cwd: root,
          env,
          timeout: 10_000,
        }).catch(() => undefined);
      }
    } finally {
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
      await removeTemporaryDirectory(temporary);
    }
  });
  await mkdir(root);
  await writeFile(
    join(root, "live.ts"),
    keyword
      ? 'export function connectionPool() { return "OLD_KEYWORD_BEFORE_FAILURE"; }\n'
      : `export function openPool() { return "connection pool ${race ? "OLD_BEFORE_FAILURE" : "LIVE_FALLBACK_EVIDENCE"}"; }\n`,
  );
  if (race) {
    await writeFile(
      join(root, "removed.ts"),
      keyword
        ? 'export function connectionPoolRemoved() { return "DELETED_KEYWORD_BEFORE_FAILURE"; }\n'
        : 'export const removed = "connection pool DELETED_BEFORE_FAILURE";\n',
    );
  }
  await writeFile(
    join(root, "semantic.ts"),
    'export function reuseConnections() { return "reuse database connections SEMANTIC_ONLY_EVIDENCE"; }\n',
  );
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  env = {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${await availablePort()}/mcp`,
    NO_COLOR: "1",
  };
  const indexed = await runCli(
    [
      "--index",
      "--mode",
      "direct",
      "--embedding",
      "qwen/text-embedding-v4",
      "--api-key",
      "fixture-key",
      "--endpoint",
      `http://127.0.0.1:${server.address().port}/embeddings`,
      "--allow-remote",
      root,
    ],
    { cwd: root, env },
  );
  assert.match(indexed.stdout, /Workspace index/);
  assert.ok(
    documentRequests > 0,
    "initial indexing embedded source successfully",
  );
  const preparedDocumentRequests = documentRequests;
  rejectQueryEmbedding = true;
  return {
    root,
    run: (args) =>
      runCli([...args, "--mode", mode], {
        cwd: root,
        env,
        timeout: 20_000,
      }),
    beforeFailure: (callback) => {
      beforeQueryFailure = callback;
    },
    setFailureStatus: (status) => {
      failureStatus = status;
    },
    queryRequests: () => failedQueryRequests,
    assertRequestsSince: (before, attempted = true) => {
      if (attempted) {
        assert.ok(
          failedQueryRequests > before,
          "this search must reach query embedding, allowing bounded retries",
        );
      } else {
        assert.equal(failedQueryRequests, before);
      }
      assert.equal(
        documentRequests,
        preparedDocumentRequests,
        "query failure must not be confused with index/model preparation",
      );
    },
  };
}

function outcome(promise) {
  return promise.then(
    (result) => ({ result }),
    (error) => ({ error }),
  );
}

function assertEmbeddingFailure(error) {
  assert.equal(error.code, 1);
  assert.match(error.stderr, /embedding|request returned an error/i);
  assert.doesNotMatch(
    error.stdout,
    /No (?:current |text )?matches|LIVE_FALLBACK_EVIDENCE|OLD_BEFORE_FAILURE/,
  );
  return true;
}

function assertFallbackWarning(stderr) {
  assert.ok(stderr.includes(fallbackWarning));
  assert.doesNotMatch(
    stderr,
    /No index found|preparing .*index|fixture_private_provider_detail/i,
  );
}

for (const mode of ["direct", "auto"]) {
  test(`ordinary ${mode} query failure can recover keyword-only source, with a final fresh rescan`, async (t) => {
    const state = await fixture(t, mode, { race: true, keyword: true });
    let changed = false;
    state.beforeFailure(async () => {
      if (changed) return;
      await writeFile(
        join(state.root, "live.ts"),
        'export function connectionPool() { return "CURRENT_KEYWORD_AFTER_FAILURE"; }\n',
      );
      await rm(join(state.root, "removed.ts"));
      changed = true;
    });
    let before = state.queryRequests();
    const result = await state.run([
      query,
      "--refresh",
      "off",
      "--allow-remote",
    ]);
    state.assertRequestsSince(before);
    assert.match(result.stdout, /matchedBy=keyword\b/);
    assert.match(result.stdout, /CURRENT_KEYWORD_AFTER_FAILURE/);
    assert.doesNotMatch(
      result.stdout,
      /OLD_KEYWORD|DELETED_KEYWORD|SEMANTIC_ONLY_EVIDENCE/,
    );
    assertFallbackWarning(result.stderr);

    before = state.queryRequests();
    await assert.rejects(
      state.run(["--vector", query, "--refresh", "off", "--allow-remote"]),
      assertEmbeddingFailure,
    );
    state.assertRequestsSince(before);

    await rm(join(state.root, "live.ts"));
    before = state.queryRequests();
    await assert.rejects(
      state.run([query, "--refresh", "off", "--allow-remote"]),
      assertEmbeddingFailure,
    );
    state.assertRequestsSince(before);
  });

  test(`ordinary ${mode} search retains live matches when query embedding fails, without changing explicit vector semantics`, async (t) => {
    const state = await fixture(t, mode);
    // No explicit route/model/trace/refresh changes the ordinary default path.
    // The unchanged, populated index ensures the injected error is a query
    // embedding failure, not initial model preparation or background indexing.
    let before = state.queryRequests();
    const ordinary = await outcome(state.run([query, "--allow-remote"]));
    state.assertRequestsSince(before);

    // Run this control before asserting ordinary success so fail-before runs
    // also prove explicit vector search already has the intended error behavior.
    before = state.queryRequests();
    await assert.rejects(
      state.run(["--vector", query, "--allow-remote"]),
      assertEmbeddingFailure,
    );
    state.assertRequestsSince(before);

    assert.equal(
      ordinary.error?.code,
      undefined,
      `ordinary search discarded its live matches: ${ordinary.error?.stderr ?? ""}`,
    );
    assert.match(ordinary.result.stdout, /live\.ts/);
    assert.match(ordinary.result.stdout, /LIVE_FALLBACK_EVIDENCE/);
    assert.doesNotMatch(ordinary.result.stdout, /SEMANTIC_ONLY_EVIDENCE/);
    assertFallbackWarning(ordinary.result.stderr);
  });

  test(`ordinary ${mode} fallback rechecks live source and preserves refresh, empty-result, and authorization boundaries`, async (t) => {
    const state = await fixture(t, mode, { race: true });
    // --allow-remote on the fixture index grants only that operation. A new
    // noninteractive query has no permit and must not contact the endpoint or
    // hide the authorization failure behind its already available live matches.
    let before = state.queryRequests();
    await assert.rejects(state.run([query]), (error) => {
      assert.equal(error.code, 1);
      assert.match(error.stderr, /authorization|cancelled|canceled/i);
      assert.doesNotMatch(error.stdout, /OLD_BEFORE_FAILURE|No matches/);
      assert.doesNotMatch(error.stderr, /showing current text matches only/i);
      return true;
    });
    state.assertRequestsSince(before, false);

    // A supplied but invalid remote credential is not an availability outage.
    // The request is authorized to leave the process, but a provider 401 must
    // still fail visibly instead of being hidden by the live-result fallback.
    state.setFailureStatus(401);
    before = state.queryRequests();
    await assert.rejects(state.run([query, "--allow-remote"]), (error) => {
      assertEmbeddingFailure(error);
      assert.doesNotMatch(error.stderr, /showing current text matches only/i);
      return true;
    });
    state.assertRequestsSince(before);
    state.setFailureStatus(503);

    before = state.queryRequests();
    await assert.rejects(
      state.run([query, "--refresh", "wait", "--allow-remote"]),
      assertEmbeddingFailure,
    );
    state.assertRequestsSince(before);

    before = state.queryRequests();
    await assert.rejects(
      state.run([missingQuery, "--allow-remote"]),
      assertEmbeddingFailure,
    );
    state.assertRequestsSince(before);

    let sourceChanged = false;
    state.beforeFailure(async () => {
      if (sourceChanged) return;
      // Query embedding starts after the initial live scan. Change the source
      // before returning the failure, so fallback must obtain fresh evidence.
      await writeFile(
        join(state.root, "live.ts"),
        'export function openPool() { return "connection pool CURRENT_AFTER_FAILURE"; }\n',
      );
      await rm(join(state.root, "removed.ts"));
      sourceChanged = true;
    });
    before = state.queryRequests();
    const raced = await outcome(
      state.run([query, "--refresh", "off", "--allow-remote"]),
    );
    assert.equal(sourceChanged, true);
    state.assertRequestsSince(before);
    assert.equal(
      raced.error?.code,
      undefined,
      `ordinary fallback failed after a concurrent edit: ${raced.error?.stderr ?? ""}`,
    );
    assert.match(raced.result.stdout, /CURRENT_AFTER_FAILURE/);
    assert.doesNotMatch(
      raced.result.stdout,
      /OLD_BEFORE_FAILURE|DELETED_BEFORE_FAILURE|removed\.ts|SEMANTIC_ONLY_EVIDENCE/,
    );
    assertFallbackWarning(raced.result.stderr);
  });
}
