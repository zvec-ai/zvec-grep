import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  writeFile,
  readFile,
  stat,
  rm,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import test from "node:test";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { createZvecGrep } from "../../dist/index.js";
import { DaemonBackend } from "../../dist/daemon/backend.js";
import { WorkspaceBindingStore } from "../../dist/engine/bindings.js";
import { createZvecGrepMcpServer } from "../../dist/mcp/tools.js";
import { printServerIndexInfo } from "../../dist/cli/format/status.js";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
import { restoreIndexedMtime } from "../helpers/mtime.mjs";

useIsolatedZvecGrepHome();

async function fixture(t, hold = false) {
  const parent = await mkdtemp(join(tmpdir(), "zg-unverified-daemon-"));
  let service, backend, client, server, release;
  const barrier = new Promise((resolve) => {
    release = resolve;
  });
  t.after(async () => {
    release();
    try {
      await client?.close();
      await server?.close();
    } finally {
      try {
        await backend?.close();
        await service?.close();
      } finally {
        await rm(parent, { recursive: true, force: true });
      }
    }
  });
  const root = join(parent, "workspace");
  await mkdir(root);
  await writeFile(
    join(root, "answer.md"),
    "# Answer\n\nalpha orchard answer\n",
  );
  await writeFile(
    join(root, "stable.md"),
    "# Stable\n\nunchanged anchor phrase\n",
  );
  service = await createZvecGrep({
    root,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await service.index();
  const info = await service.info();
  const id = info.workspaceIndex.id;
  await service.close();
  service = undefined;
  const bindings = new WorkspaceBindingStore();
  bindings.invalidate(id, root);
  assert.equal(
    bindings.matches(id, root),
    false,
    "fixture must start unverified",
  );
  const model = new CountingEmbeddingModel();
  let indexCalls = 0;
  backend = new DaemonBackend({
    version: "test",
    modelPoolOptions: { createModel: () => model },
    createService: async (options) => {
      const current = await createZvecGrep({
        ...options,
        embeddingModel: model,
      });
      const index = current.index.bind(current);
      current.index = async (...args) => {
        indexCalls++;
        if (hold) await barrier;
        return index(...args);
      };
      return current;
    },
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {},
    }),
  });
  server = createZvecGrepMcpServer(backend, "test", { toolset: "full" });
  client = new Client({ name: "unverified-test", version: "1" });
  const [left, right] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(left), server.connect(right)]);
  const call = async (name, args = {}) => {
    const result = await client.callTool({
      name: `zvec_grep_${name}`,
      arguments: { root, ...args },
    });
    assert.notEqual(result.isError, true, JSON.stringify(result));
    return result;
  };
  return {
    root,
    id,
    bindings,
    model,
    backend,
    call,
    release,
    indexCalls: () => indexCalls,
  };
}
const text = (result) =>
  result.content.map((item) => item.text ?? "").join("\n");
async function verified(f) {
  const end = Date.now() + 5000;
  while (!f.bindings.matches(f.id, f.root)) {
    assert(
      Date.now() < end,
      "background reconciliation must establish the binding",
    );
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

test("F1 status exposes an unverified binding through the actual MCP interface", async (t) => {
  const f = await fixture(t);
  const result = await f.call("index_status");
  assert.equal(result.structuredContent.persistent.unverified, true);
});

test("F1 CLI status presents the MCP unverified index as needing an update", async (t) => {
  const f = await fixture(t);
  const result = await f.call("index_status");
  const lines = [],
    log = console.log;
  let state;
  try {
    console.log = (...args) => lines.push(args.join(" "));
    state = printServerIndexInfo(result.structuredContent, { color: false });
  } finally {
    console.log = log;
  }
  assert.equal(state, "stale");
  assert.match(lines.join("\n"), /needs an update/);
});

test("F1 default MCP search schedules reconciliation and reuses unchanged document vectors", async (t) => {
  const f = await fixture(t, true);
  const result = await f.call("search", { query: "orchard answer" });
  assert.match(text(result), /freshness: possibly_stale/);
  assert.match(text(result), /background_refresh: (queued|running)/);
  f.release();
  await verified(f);
  assert.equal(f.indexCalls(), 1);
  assert.equal(
    f.model.counts.document,
    0,
    "unchanged documents must reuse vectors",
  );
  const status = await f.call("index_status");
  assert.equal(status.structuredContent.persistent.unverified, false);
});

test("F1 wait_for_fresh verifies the binding before returning with zero document embeddings", async (t) => {
  const f = await fixture(t);
  const result = await f.call("search", {
    query: "orchard answer",
    freshness: "wait_for_fresh",
  });
  assert.equal(
    f.bindings.matches(f.id, f.root),
    true,
    "a fresh response must have a verified binding",
  );
  assert.equal(f.indexCalls(), 1);
  assert.equal(f.model.counts.document, 0);
  assert.match(text(result), /freshness: fresh/);
  assert.match(text(result), /alpha orchard answer/);
});

test("F1 disabled automatic refresh keeps the binding unverified and reports stale results", async (t) => {
  const f = await fixture(t);
  const result = await f.call("search", {
    query: "orchard answer",
    autoUpdate: false,
  });
  assert.match(text(result), /freshness: possibly_stale/);
  assert.equal(f.indexCalls(), 0);
  assert.equal(f.bindings.matches(f.id, f.root), false);
  assert.equal(f.model.counts.document, 0);
});

test("F1 unverified reconciliation finds a same-stat edit and preserves the other document", async (t) => {
  const f = await fixture(t);
  const file = join(f.root, "answer.md"),
    before = await stat(file);
  await writeFile(
    file,
    (await readFile(file, "utf8")).replace("alpha", "omega"),
  );
  await restoreIndexedMtime(file, before.mtimeMs);
  assert.equal((await stat(file)).size, before.size);
  const result = await f.call("search", {
    query: "omega orchard answer unchanged anchor",
    freshness: "wait_for_fresh",
  });
  assert.match(text(result), /omega orchard answer/);
  assert.doesNotMatch(text(result), /alpha orchard answer/);
  assert.match(text(result), /unchanged anchor phrase/);
  assert.equal(
    f.model.counts.document,
    1,
    "only the changed document is embedded",
  );
  assert.equal(f.bindings.matches(f.id, f.root), true);
});
