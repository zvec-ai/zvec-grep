import assert from "node:assert/strict";
import { mkdir, rm, utimes, writeFile } from "node:fs/promises";
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

for (const mode of ["direct", "auto"]) {
  test(`ordinary ${mode} search refreshes live evidence after a concurrent edit during embedding`, async (t) => {
    const temporary = await createTemporaryDirectory(t, "zvec-live-race-", {
      cleanup: false,
    });
    const root = join(temporary, "repo");
    const home = join(temporary, "home");
    const query = "connection pool";
    let releaseEmbedding;
    const embeddingGate = new Promise((resolve) => {
      releaseEmbedding = resolve;
    });
    let observedQuery;
    const queryRequested = new Promise((resolve) => {
      observedQuery = resolve;
    });
    let gateArmed = false;
    let queryOutcome;
    let env;
    let gateTimeout;
    const server = createServer(async (request, response) => {
      try {
        const chunks = [];
        for await (const chunk of request) chunks.push(chunk);
        const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        const inputs = Array.isArray(body.input) ? body.input : [];
        if (gateArmed && inputs.includes(query)) {
          observedQuery();
          await embeddingGate;
        }
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
      clearTimeout(gateTimeout);
      releaseEmbedding();
      try {
        // Attach a rejection handler when starting the subprocess below, so a
        // failed gate cannot leave an unhandled rejection or a held request.
        await queryOutcome;
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
      join(root, "changed.ts"),
      'export function openPool() { return "connection pool OLD_DURING_QUERY"; }\n',
    );
    await writeFile(
      join(root, "deleted.ts"),
      'export const removed = "connection pool DELETED_DURING_QUERY";\n',
    );
    await writeFile(
      join(root, "stable.ts"),
      'export function reuseConnections() { return "reuse database connections SEMANTIC_STABLE"; }\n',
    );
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", resolve);
    });
    const endpoint = `http://127.0.0.1:${server.address().port}/embeddings`;
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
        root,
      ],
      { cwd: root, env },
    );
    gateArmed = true;
    queryOutcome = runCli(
      [query, "--mode", mode, "--refresh", "off", "--allow-remote"],
      { cwd: root, env, timeout: 20_000 },
    ).then(
      (result) => ({ result }),
      (error) => ({ error }),
    );
    await Promise.race([
      queryRequested,
      queryOutcome.then((outcome) => {
        if (outcome.error) throw outcome.error;
        throw new Error("Search completed without reaching the embedding gate");
      }),
      new Promise((_, reject) => {
        gateTimeout = setTimeout(
          () => reject(new Error("Search did not reach the embedding gate")),
          10_000,
        );
      }),
    ]);
    clearTimeout(gateTimeout);
    // The ordinary CLI has already completed its initial live scan when it
    // requests query embedding. Mutate only now, while the response is held;
    // no sleep or filesystem timing race is needed to place these edits.
    await writeFile(
      join(root, "changed.ts"),
      'export function openPool() { return "connection pool CURRENT_AFTER_QUERY"; }\n',
    );
    const modified = new Date(Date.now() + 5_000);
    await utimes(join(root, "changed.ts"), modified, modified);
    await rm(join(root, "deleted.ts"));
    await writeFile(
      join(root, "added.ts"),
      'export function newPool() { return "connection pool ADDED_DURING_QUERY"; }\n',
    );
    releaseEmbedding();
    const outcome = await queryOutcome;
    if (outcome.error) throw outcome.error;
    const { stdout } = outcome.result;
    assert.doesNotMatch(stdout, /OLD_DURING_QUERY|DELETED_DURING_QUERY/);
    assert.match(stdout, /CURRENT_AFTER_QUERY/);
    assert.match(stdout, /ADDED_DURING_QUERY/);
    assert.match(stdout, /SEMANTIC_STABLE/);
    assert.equal((stdout.match(/#[0-9]+[^\n]*changed\.ts/g) ?? []).length, 1);
    assert.doesNotMatch(stdout, /deleted\.ts/);
  });
}
