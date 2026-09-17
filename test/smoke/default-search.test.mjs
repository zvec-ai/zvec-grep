import assert from "node:assert/strict";
import { cp, mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import test from "node:test";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";
import { DaemonClient } from "../../dist/client/daemon-client.js";
import { readInstanceRecord } from "../../dist/daemon/server-controller.js";

test(
  "default semantic search recovers a failed initial index once the local model becomes available",
  { skip: !process.env.ZVEC_GREP_MODEL_CACHE },
  async (t) => {
    const temporary = await createTemporaryDirectory(
      t,
      "zvec-recover-initial-",
      {
        cleanup: false,
      },
    );
    const root = join(temporary, "repo");
    const home = join(temporary, "home");
    const cache = join(temporary, "models");
    await mkdir(root);
    await writeFile(
      join(root, "retry.ts"),
      "/** Retry failed network requests with exponential backoff. */\nexport async function retryRequest(send) { try { return await send(); } catch { await new Promise(resolve => setTimeout(resolve, 100)); return send(); } }\n",
    );
    const probe = createServer();
    await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
    const port = probe.address().port;
    await new Promise((resolve) => probe.close(resolve));
    const serverUrl = `http://127.0.0.1:${port}/mcp`;
    const env = {
      HOME: home,
      USERPROFILE: home,
      ZVEC_GREP_HOME: home,
      NO_COLOR: "1",
      ZVEC_GREP_SERVER_URL: serverUrl,
      ZVEC_GREP_EMBEDDING: "local/potion-code-16m-v2",
      ZVEC_GREP_MODEL_CACHE: cache,
      NODE_OPTIONS: `--import=${new URL("../helpers/failing-model-download.mjs", import.meta.url).href}`,
    };
    t.after(async () => {
      await runCli(["--server", "off"], { cwd: root, env });
      await removeTemporaryDirectory(temporary);
    });
    const first = await runCli(["network failure backoff"], { cwd: root, env });
    assert.match(first.stdout, /retry\.ts/);
    assert.match(first.stdout, /matchedBy=keyword/);
    assert.match(first.stdout, /network requests with exponential backoff/);
    assert.doesNotMatch(first.stdout, /No (?:text )?matches/);
    assert.match(first.stderr, /semantic index preparation failed/);
    const daemon = await readInstanceRecord(home);
    const client = new DaemonClient({ serverUrl, home });
    const failed = await client.callTool("zvec_grep_index_status", { root });
    assert.equal(
      failed.indexed,
      true,
      "collection exists despite failed initialization",
    );
    assert.equal(failed.runtime.job_state, "failed");
    assert.equal(failed.persistent.files.indexed, 0);
    const relativeModelCache = join(
      "model2vec",
      "minishlab--potion-code-16M-v2",
    );
    await cp(
      join(process.env.ZVEC_GREP_MODEL_CACHE, relativeModelCache),
      join(cache, relativeModelCache),
      { recursive: true },
    );
    const recovered = await runCli(["network failure backoff"], {
      cwd: root,
      // Changing the default must not replace an already initialized index's
      // model. Only the original model is available in this offline cache.
      env: { ...env, ZVEC_GREP_EMBEDDING: "local/potion-multilingual-128m" },
      timeout: 15_000,
    });
    assert.match(recovered.stdout, /^#1 .*retry\.ts/m);
    assert.doesNotMatch(recovered.stderr, /text search only|not ready|failed/);
    assert.equal((await readInstanceRecord(home)).pid, daemon.pid);
    const ready = await client.callTool("zvec_grep_index_status", { root });
    assert.equal(ready.runtime.job_state, "succeeded");
    assert.ok(ready.persistent.files.indexed > 0);
    assert.equal(
      ready.persistent.workspace_index.embedding.model,
      "potion-code-16m-v2",
    );
  },
);

test(
  "first semantic-only query can finish a small local index without another invocation",
  { skip: !process.env.ZVEC_GREP_MODEL_CACHE },
  async (t) => {
    const temporary = await createTemporaryDirectory(
      t,
      "zvec-first-semantic-",
      { cleanup: false },
    );
    const root = join(temporary, "repo");
    const home = join(temporary, "home");
    await mkdir(root);
    await writeFile(
      join(root, "retry.ts"),
      [
        "/** Retry failed network requests with exponential backoff. */",
        "export async function retryRequest(send) {",
        "  try { return await send(); } catch {",
        "    await new Promise(resolve => setTimeout(resolve, 100));",
        "    return await send();",
        "  }",
        "}",
      ].join("\n"),
    );
    await writeFile(
      join(root, "payment.ts"),
      "export function refundPayment(payment) { return payment.refund(); }\n",
    );
    const probe = createServer();
    await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
    const port = probe.address().port;
    await new Promise((resolve) => probe.close(resolve));
    const env = {
      HOME: home,
      USERPROFILE: home,
      ZVEC_GREP_HOME: home,
      NO_COLOR: "1",
      ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
      ZVEC_GREP_EMBEDDING: "local/potion-code-16m-v2",
      ZVEC_GREP_MODEL_CACHE: process.env.ZVEC_GREP_MODEL_CACHE,
    };
    t.after(async () => {
      await runCli(["--server", "off"], { cwd: root, env });
      await removeTemporaryDirectory(temporary);
    });
    const start = performance.now();
    const first = await runCli(["network failure backoff"], {
      cwd: root,
      env,
      timeout: 15_000,
    });
    assert.match(first.stdout, /^#1 .*retry\.ts/m);
    assert.doesNotMatch(first.stderr, /text search only|not ready|failed/);
    assert.doesNotMatch(first.stdout, /No text matches|No matches\./);
    t.diagnostic(
      JSON.stringify({
        firstSemanticMs: Math.round(performance.now() - start),
      }),
    );
  },
);

test(
  "real local model builds after CLI exit and serves subsequent default searches",
  {
    skip: !process.env.ZVEC_GREP_MODEL_CACHE,
  },
  async (t) => {
    const temporary = await createTemporaryDirectory(t, "zvec-real-default-", {
      cleanup: false,
    });
    const root = join(temporary, "repo");
    const home = join(temporary, "home");
    await mkdir(root);
    await writeFile(
      join(root, "retry.ts"),
      [
        "/** Retry failed network requests with exponential backoff. */",
        "export async function retryRequest(send, attempts = 3) {",
        "  for (let attempt = 0; attempt < attempts; attempt++) {",
        "    try { return await send(); } catch (error) {",
        "      if (attempt + 1 === attempts) throw error;",
        "      await new Promise(resolve => setTimeout(resolve, 100 * 2 ** attempt));",
        "    }",
        "  }",
        "}",
      ].join("\n"),
    );
    await writeFile(
      join(root, "payment.ts"),
      "export function refundPayment(payment) { return payment.refund(); }\n",
    );
    await writeFile(
      join(root, "session.ts"),
      "export function expireSession(session) { session.token = null; }\n",
    );
    const probe = createServer();
    await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
    const port = probe.address().port;
    await new Promise((resolve) => probe.close(resolve));
    const serverUrl = `http://127.0.0.1:${port}/mcp`;
    const env = {
      HOME: home,
      USERPROFILE: home,
      ZVEC_GREP_HOME: home,
      NO_COLOR: "1",
      ZVEC_GREP_SERVER_URL: serverUrl,
      ZVEC_GREP_EMBEDDING: "local/potion-code-16m-v2",
      ZVEC_GREP_MODEL_CACHE: process.env.ZVEC_GREP_MODEL_CACHE,
    };
    t.after(async () => {
      await runCli(["--server", "off"], { cwd: root, env }).catch(
        () => undefined,
      );
      await removeTemporaryDirectory(temporary);
    });
    const started = performance.now();
    const cold = await runCli(["retry", "failed", "network", "requests"], {
      cwd: root,
      env,
      timeout: 5_000,
    });
    const coldMs = performance.now() - started;
    assert.match(cold.stderr, /text search only.*background/);
    assert.match(cold.stdout, /retry\.ts/);
    const daemon = await readInstanceRecord(home);
    assert.ok(daemon?.ready);
    const client = new DaemonClient({ serverUrl, home });
    const deadline = Date.now() + 30_000;
    let indexed = false;
    while (Date.now() < deadline) {
      const status = await client.callTool("zvec_grep_index_status", { root });
      assert.ok(
        !["failed", "cancelled"].includes(status.runtime?.job_state),
        "Background index preparation did not succeed",
      );
      if (status.indexed && status.runtime?.job_state === "succeeded") {
        indexed = true;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    assert.ok(
      indexed,
      "the actual local embedding index must finish after the initial CLI exits",
    );
    const indexReadyMs = performance.now() - started;
    const warmStarted = performance.now();
    const warm = await runCli(["network", "failure", "backoff"], {
      cwd: root,
      env,
    });
    assert.match(warm.stdout, /retry\.ts/);
    assert.ok(
      !warm.stdout.includes("payment.ts") ||
        warm.stdout.indexOf("retry.ts") < warm.stdout.indexOf("payment.ts"),
    );
    assert.doesNotMatch(warm.stderr, /text search only|No index found/);
    assert.equal((await readInstanceRecord(home)).pid, daemon.pid);
    t.diagnostic(
      JSON.stringify({
        coldMs: Math.round(coldMs),
        indexReadyMs: Math.round(indexReadyMs),
        warmMs: Math.round(performance.now() - warmStarted),
      }),
    );
  },
);
