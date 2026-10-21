import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { after } from "node:test";
import { ZVecOpen } from "@zvec/zvec";
import { resolveWorkspaceIndexStoragePaths } from "../../dist/engine/storage/layout.js";
import { acquireReadWriteLock } from "../../dist/engine/utils/lock.js";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { createZvecGrepMcpServer } from "../../dist/mcp/tools.js";
import { DaemonBackend } from "../../dist/daemon/backend.js";
import { createZvecGrep } from "../../dist/index.js";
import { exportWorkspaceIndex } from "../../dist/engine/transfer/index.js";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";

useIsolatedZvecGrepHome();
const operations = ["migrate", "export", "import"];
const tool = (operation) => `zvec_grep_index_${operation}`;
const content = "# Beacon\n\nsealed kernel lantern phrase\n";

let templatePromise;
let templateParent;
after(async () => {
  if (templateParent)
    await rm(templateParent, { recursive: true, force: true });
});

// Only fixture construction is shared. Every operation still uses its own
// closed database copies, artifact, server, destination and locks.
function template() {
  return (templatePromise ??= (async () => {
    templateParent = await mkdtemp(join(tmpdir(), "zg-mcp-template-"));
    const original = await buildSource(templateParent);
    const legacyHome = join(original.root, ".zvec-grep-legacy");
    await buildLegacyHome(original.root, legacyHome, original.id);
    const artifactPath = join(templateParent, "artifact");
    await exportWorkspaceIndex({
      sourceHome: original.sourceHome,
      artifactPath,
    });
    return { ...original, legacyHome, artifactPath };
  })());
}

const copyOptions = {
  recursive: true,
  preserveTimestamps: true,
  force: false,
  errorOnExist: true,
};

async function fixture(t, toolset = "full") {
  const parent = await mkdtemp(join(tmpdir(), "zg-mcp-portability-"));
  let client, server, backend;
  t.after(async () => {
    try {
      await client?.close();
      await server?.close();
    } finally {
      try {
        await backend?.close();
      } finally {
        await rm(parent, { recursive: true, force: true });
      }
    }
  });
  const model = new CountingEmbeddingModel();
  backend = new DaemonBackend({
    version: "test",
    modelPoolOptions: { createModel: () => model },
    createService: (options) =>
      createZvecGrep({ ...options, embeddingModel: model }),
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {},
    }),
  });
  server = createZvecGrepMcpServer(backend, "test", { toolset });
  client = new Client({ name: "portability-test", version: "1" });
  const [left, right] = InMemoryTransport.createLinkedPair();
  await Promise.all([client.connect(left), server.connect(right)]);
  return { parent, model, backend, client, server };
}

async function requireTool(client, operation) {
  const found = (await client.listTools()).tools.find(
    (item) => item.name === tool(operation),
  );
  assert.ok(found, `${tool(operation)} must be discovered through MCP`);
  return found;
}

async function call(client, operation, args, options) {
  const result = await client.callTool(
    { name: tool(operation), arguments: { confirm: true, ...args } },
    options,
  );
  assert.notEqual(result.isError, true, JSON.stringify(result));
  assert.equal(result.structuredContent.state, "succeeded");
  return result.structuredContent.result;
}

async function source(parent) {
  const original = await template();
  const root = join(parent, "source");
  await cp(original.root, root, {
    ...copyOptions,
    filter: (path) => path !== original.legacyHome,
  });
  return { root, sourceHome: join(root, ".zvec-grep"), id: original.id };
}

async function legacySource(original) {
  const home = join(original.root, ".zvec-grep-legacy");
  await cp((await template()).legacyHome, home, copyOptions);
  return home;
}

async function buildSource(parent) {
  const root = join(parent, "source");
  await documents(root);
  const service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  try {
    await service.index();
  } finally {
    await service.close();
  }
  const sourceHome = join(root, ".zvec-grep");
  const manifest = JSON.parse(
    await readFile(join(sourceHome, "manifest.json"), "utf8"),
  );
  return { root, sourceHome, id: manifest.id };
}
async function documents(root) {
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "beacon.md"), content);
  await writeFile(
    join(root, "stable.md"),
    "# Anchor\n\nquiet harbor anchor phrase\n",
  );
}
async function inventory(root) {
  const result = {};
  async function visit(dir, prefix = "") {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      if (entry.name === "locks") continue;
      const key = `${prefix}${entry.name}`;
      if (entry.isDirectory()) await visit(join(dir, entry.name), `${key}/`);
      else
        result[key] = createHash("sha256")
          .update(await readFile(join(dir, entry.name)))
          .digest("hex");
    }
  }
  await visit(root);
  return result;
}
function vectors(home) {
  const collection = ZVecOpen(
    resolveWorkspaceIndexStoragePaths(home).indexPath,
    { readOnly: true, enableMMAP: false },
  );
  try {
    return [...collection.iterDocsSync({ includeVector: true })]
      .map((doc) => ({ id: doc.id, vector: Array.from(doc.vectors.embedding) }))
      .sort((a, b) => a.id.localeCompare(b.id));
  } finally {
    collection.closeSync();
  }
}
async function searchAndReconcile(f, root, id) {
  const result = await f.client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "sealed kernel lantern", autoUpdate: false },
  });
  assert.notEqual(result.isError, true, JSON.stringify(result));
  assert.match(JSON.stringify(result.content), /beacon.md/);
  assert.match(JSON.stringify(result.content), /sealed kernel lantern phrase/);
  const indexed = await f.client.callTool({
    name: "zvec_grep_index",
    arguments: { root, wait: true },
  });
  assert.equal(
    indexed.structuredContent.state,
    "succeeded",
    JSON.stringify(indexed),
  );
  await f.backend.close();
  assert.equal(
    f.model.counts.document,
    0,
    "unchanged documents must reuse vectors through close",
  );
  assert.ok(f.model.counts.query > 0, "the query model must run");
  assert.equal(
    JSON.parse(await readFile(join(root, ".zvec-grep/manifest.json"), "utf8"))
      .id,
    id,
  );
}

test("MCP portability discovery adds three full tools and preserves the default search toolset", async (t) => {
  const full = await fixture(t);
  for (const operation of operations) {
    const found = await requireTool(full.client, operation);
    assert.equal(found.annotations.readOnlyHint, false);
    assert.equal(found.annotations.destructiveHint, false);
  }
  const agent = await fixture(t, "agent");
  assert.deepEqual(
    (await agent.client.listTools()).tools.map((item) => item.name),
    ["zvec_grep_search"],
  );
});

test("MCP portability descriptions require a user request and server-visible paths", async (t) => {
  const f = await fixture(t);
  for (const operation of operations) {
    const found = await requireTool(f.client, operation);
    assert.match(found.description, /explicit user request/i);
    assert.match(found.description, /server/i);
    assert.match(found.description, /separate operation/i);
    assert.equal(found.inputSchema.properties.confirm.const, true);
  }
});

for (const operation of operations) {
  test(`MCP ${operation} rejects relative paths, missing confirmation and unknown fields`, async (t) => {
    const f = await fixture(t);
    await requireTool(f.client, operation);
    const paths =
      operation === "export"
        ? {
            sourceHome: join(f.parent, "source"),
            artifactPath: join(f.parent, "artifact"),
          }
        : operation === "migrate"
          ? {
              sourceHome: join(f.parent, "source"),
              destinationRoot: join(f.parent, "dest"),
            }
          : {
              artifactPath: join(f.parent, "artifact"),
              destinationRoot: join(f.parent, "dest"),
            };
    for (const args of [
      paths,
      { ...paths, confirm: false },
      { ...paths, confirm: true, extra: "ignored?" },
      { ...paths, confirm: true, [Object.keys(paths)[0]]: "relative" },
    ]) {
      const result = await f.client.callTool({
        name: tool(operation),
        arguments: args,
      });
      assert.equal(result.isError, true);
      assert.match(
        JSON.stringify(result.content),
        /valid|confirm|absolute|recognized/i,
      );
    }
    assert.deepEqual(
      await readdir(f.parent),
      [],
      "validation must precede all filesystem operations",
    );
  });
}

test("MCP fixtures reuse one indexed source and keep copied bytes private", async (t) => {
  const f = await fixture(t);
  let embedded = 0;
  const embed = FakeEmbeddingModel.prototype.doEmbed;
  t.mock.method(FakeEmbeddingModel.prototype, "doEmbed", function (...args) {
    embedded += args[0].length;
    return Reflect.apply(embed, this, args);
  });
  const first = await source(join(f.parent, "first"));
  const second = await source(join(f.parent, "second"));
  assert.equal(embedded, 2, "fixture setup must index the two documents once");
  assert.notEqual(first.root, second.root);
  const before = await inventory(second.root);
  assert.deepEqual(await inventory(first.root), before);
  const nativeHome = resolveWorkspaceIndexStoragePaths(
    first.sourceHome,
  ).filesPath;
  const nativeFile = Object.keys(await inventory(nativeHome))[0];
  assert.ok(nativeFile, "the independence check must change native storage");
  await writeFile(join(first.root, "beacon.md"), "changed fixture document");
  await writeFile(join(nativeHome, nativeFile), "changed fixture storage");
  assert.deepEqual(
    await inventory(second.root),
    before,
    "fixture writes must not reach another copy",
  );
  const third = await source(join(f.parent, "third"));
  assert.deepEqual(
    await inventory(third.root),
    before,
    "fixture writes must not reach the template",
  );
  assert.equal(embedded, 2, "copying another fixture must not rebuild it");
});

test("MCP migration preserves legacy workspace identity, content and all vectors", async (t) => {
  const f = await fixture(t);
  await requireTool(f.client, "migrate");
  const original = await source(f.parent);
  const legacy = await legacySource(original);
  const before = await inventory(legacy);
  const destinationRoot = join(f.parent, "migrated");
  await documents(destinationRoot);
  const result = await call(f.client, "migrate", {
    sourceHome: legacy,
    destinationRoot,
  });
  assert.equal(result.indexId, original.id);
  assert.equal(result.filesConverted, 2);
  assert.equal(result.verification.vectorsExact, true);
  assert.equal(result.verification.vectorsSampled, false);
  assert.deepEqual(result.missingFiles, []);
  assert.deepEqual(await inventory(legacy), before);
  assert.deepEqual(
    vectors(join(destinationRoot, ".zvec-grep")),
    vectors(original.sourceHome),
  );
  await searchAndReconcile(f, destinationRoot, original.id);
});

test("MCP export preserves its source and sends ordered progress notifications", async (t) => {
  const f = await fixture(t);
  await requireTool(f.client, "export");
  const original = await source(f.parent);
  const before = await inventory(original.sourceHome);
  const artifactPath = join(f.parent, "artifact");
  const progress = [];
  const result = await call(
    f.client,
    "export",
    { sourceHome: original.sourceHome, artifactPath },
    { onprogress: (item) => progress.push(item) },
  );
  assert.equal(result.indexId, original.id);
  assert.equal(result.filesExported, 2);
  assert.deepEqual(await inventory(original.sourceHome), before);
  assert.ok(progress.length >= 3, "progress must be received through MCP");
  assert.match(progress.at(-1).message, /complete/i);
  for (let i = 1; i < progress.length; i++)
    assert.ok(progress[i].progress > progress[i - 1].progress);
  assert.equal(
    JSON.parse(await readFile(join(artifactPath, "format.json"), "utf8"))
      .indexId,
    original.id,
  );
});

test("MCP import preserves identity, expected content and zero document embeddings", async (t) => {
  const f = await fixture(t);
  await requireTool(f.client, "import");
  const original = await source(f.parent);
  const artifactPath = join(f.parent, "artifact");
  await call(f.client, "export", {
    sourceHome: original.sourceHome,
    artifactPath,
  });
  const before = await inventory(artifactPath);
  const destinationRoot = join(f.parent, "imported");
  await documents(destinationRoot);
  const result = await call(f.client, "import", {
    artifactPath,
    destinationRoot,
  });
  assert.equal(result.indexId, original.id);
  assert.equal(result.filesImported, 2);
  assert.equal(result.verification.vectorsExact, true);
  assert.equal(result.verification.vectorsSampled, false);
  assert.deepEqual(result.missingFiles, []);
  assert.deepEqual(await inventory(artifactPath), before);
  assert.deepEqual(
    vectors(join(destinationRoot, ".zvec-grep")),
    vectors(original.sourceHome),
  );
  await searchAndReconcile(f, destinationRoot, original.id);
});

async function preparedOperation(t, operation) {
  const f = await fixture(t);
  await requireTool(f.client, operation);
  const original = await source(f.parent);
  let sourceHome = original.sourceHome;
  const artifactPath = join(f.parent, "artifact");
  const destinationRoot = join(f.parent, "destination");
  await documents(destinationRoot);
  if (operation === "migrate") {
    sourceHome = await legacySource(original);
  } else if (operation === "import") {
    await cp((await template()).artifactPath, artifactPath, copyOptions);
  }
  return {
    ...f,
    original,
    input:
      operation === "export"
        ? { sourceHome, artifactPath }
        : operation === "migrate"
          ? { sourceHome, destinationRoot }
          : { artifactPath, destinationRoot },
    source: operation === "import" ? artifactPath : sourceHome,
    destination:
      operation === "export"
        ? artifactPath
        : join(destinationRoot, ".zvec-grep"),
  };
}

async function failure(f, operation, code) {
  const result = await f.client.callTool({
    name: tool(operation),
    arguments: { ...f.input, confirm: true },
  });
  assert.equal(result.isError, true, JSON.stringify(result));
  assert.equal(result.structuredContent.state, "failed");
  assert.equal(result.structuredContent.operation, operation);
  assert.match(result.structuredContent.error.code, code);
  assert.ok(result.structuredContent.error.message.length > 0);
  return result;
}

function ownedResources(root) {
  if (!existsSync(root)) return [];
  const found = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (
      entry.name === "INCOMPLETE" ||
      entry.name === "lock.json" ||
      entry.name.startsWith("staging-")
    )
      found.push(path);
    if (entry.isDirectory()) found.push(...ownedResources(path));
  }
  return found;
}

async function waitForCleanup(f) {
  const deadline = Date.now() + 10_000;
  while (ownedResources(f.parent).length && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.deepEqual(
    ownedResources(f.parent),
    [],
    "the operation must release its locks, marker and staging before fixture teardown",
  );
}

test("MCP survives a fatal portability process exit and retains the incomplete destination", async (t) => {
  const f = await preparedOperation(t, "migrate");
  const before = await inventory(f.source);
  let stopped = false;
  const result = await f.client.callTool(
    { name: tool("migrate"), arguments: { ...f.input, confirm: true } },
    {
      onprogress: (progress) => {
        if (progress.message.startsWith("write:") && !stopped) {
          const lock = JSON.parse(
            readFileSync(
              join(f.destination, "locks/home.write/lock.json"),
              "utf8",
            ),
          );
          stopped = true;
          // Node's test runner isolates this file. On the old thread path this
          // deliberately kills that test process, proving the missing boundary.
          process.stderr.write(
            "CONTROL: fatal portability exit reached the write stage\n",
          );
          process.kill(lock.pid, "SIGKILL");
        }
      },
    },
  );
  assert.equal(stopped, true);
  assert.equal(result.isError, true);
  assert.match(JSON.stringify(result), /TRANSFER_PROCESS_FAILED/);
  assert.ok(existsSync(join(f.destination, "INCOMPLETE")));
  assert.deepEqual(await inventory(f.source), before);
  await requireTool(f.client, "migrate");
  const exported = await call(f.client, "export", {
    sourceHome: f.source,
    artifactPath: join(f.parent, "after-crash"),
  });
  assert.equal(
    exported.indexId,
    f.original.id,
    "the same MCP server must serve the next operation",
  );
});

test("MCP import rejects malformed entity data and preserves the artifact and destination", async (t) => {
  const f = await preparedOperation(t, "import");
  const file = join(f.source, "entities.jsonl");
  const valid = await readFile(file, "utf8");
  const entities = valid.trim().split("\n").map(JSON.parse);
  entities[0].fields.range_json = "null";
  await writeFile(file, entities.map(JSON.stringify).join("\n") + "\n");
  const before = await inventory(f.parent);
  await failure(f, "import", /ZVEC_GREP\.ENGINE\./);
  await waitForCleanup(f);
  assert.deepEqual(await inventory(f.parent), before);
  await writeFile(file, valid);
  assert.equal(
    (await call(f.client, "import", f.input)).indexId,
    f.original.id,
  );
});

for (const operation of operations) {
  test(`MCP ${operation} rejects an occupied destination without changing existing data`, async (t) => {
    const f = await preparedOperation(t, operation);
    await mkdir(f.destination, { recursive: true });
    await writeFile(
      join(f.destination, "keep.txt"),
      "foreign destination data",
    );
    const before = await inventory(f.parent);
    await failure(f, operation, /ZVEC_GREP\.ENGINE\.RESERVATION\.FAILED/);
    await waitForCleanup(f);
    assert.deepEqual(await inventory(f.parent), before);
  });

  for (const side of ["source", "destination"]) {
    test(`MCP ${operation} preserves a competing ${side} lock and allows a retry`, async (t) => {
      const f = await preparedOperation(t, operation);
      const lock = acquireReadWriteLock(
        join(f[side], "locks", "home"),
        "write",
        { operation: "MCP competing writer control" },
      );
      t.after(() => lock.release());
      const record = await readFile(join(lock.path, "lock.json"), "utf8");
      const before = await inventory(f.parent);
      await failure(f, operation, /LOCK/);
      assert.equal(
        await readFile(join(lock.path, "lock.json"), "utf8"),
        record,
        "a foreign writer must retain its exact lock",
      );
      assert.deepEqual(await inventory(f.parent), before);
      assert.deepEqual(ownedResources(f.parent), [
        join(lock.path, "lock.json"),
      ]);
      assert.equal(lock.release(), true);
      assert.equal(
        (await call(f.client, operation, f.input)).indexId,
        f.original.id,
      );
      await waitForCleanup(f);
    });
  }

  test(`MCP ${operation} cancellation before publication removes owned resources and allows a retry`, async (t) => {
    const f = await preparedOperation(t, operation);
    const before = await inventory(f.parent);
    const controller = new AbortController();
    let cancelledAtPublication = false;
    let stagedAtCancellation = [];
    const request = f.client.callTool(
      {
        name: tool(operation),
        arguments: { ...f.input, confirm: true },
      },
      {
        signal: controller.signal,
        onprogress: (progress) => {
          if (progress.message.startsWith("publish:")) {
            stagedAtCancellation = ownedResources(f.destination);
            cancelledAtPublication = true;
            controller.abort(
              new Error("MCP test cancelled before publication"),
            );
          }
        },
      },
    );
    await assert.rejects(request, /cancelled before publication/);
    assert.equal(
      cancelledAtPublication,
      true,
      "the actual MCP progress notification must trigger cancellation",
    );
    assert.ok(
      stagedAtCancellation.some((path) => path.endsWith("INCOMPLETE")),
      "a reserved result must exist before cancellation",
    );
    assert.ok(
      stagedAtCancellation.some((path) => /staging-/.test(path)),
      "the test must exercise populated staging",
    );
    await waitForCleanup(f);
    assert.deepEqual(
      await inventory(f.parent),
      before,
      "cancellation must preserve all pre-existing data and publish nothing",
    );
    assert.equal(
      (await call(f.client, operation, f.input)).indexId,
      f.original.id,
    );
    await waitForCleanup(f);
  });

  test(`MCP ${operation} connection loss before publication removes owned resources`, async (t) => {
    const f = await preparedOperation(t, operation);
    const before = await inventory(f.parent);
    let disconnectedWithStaging = false;
    let closing;
    const request = f.client.callTool(
      {
        name: tool(operation),
        arguments: { ...f.input, confirm: true },
      },
      {
        onprogress: (progress) => {
          if (progress.message.startsWith("publish:")) {
            disconnectedWithStaging = ownedResources(f.destination).some(
              (path) => path.endsWith("INCOMPLETE"),
            );
            closing = f.client.close();
          }
        },
      },
    );
    await assert.rejects(request, /closed/i);
    await closing;
    assert.equal(disconnectedWithStaging, true);
    await waitForCleanup(f);
    assert.deepEqual(await inventory(f.parent), before);
  });
}

for (const change of ["modified", "deleted"]) {
  test(`MCP imported index reconciles a ${change} file and preserves the unrelated file`, async (t) => {
    const f = await preparedOperation(t, "import");
    await call(f.client, "import", f.input);
    const root = f.input.destinationRoot;
    const index = async () => {
      const result = await f.client.callTool({
        name: "zvec_grep_index",
        arguments: { root, wait: true },
      });
      assert.equal(
        result.structuredContent.state,
        "succeeded",
        JSON.stringify(result),
      );
    };
    await index();
    assert.equal(
      f.model.counts.document,
      0,
      "initial reconciliation must reuse the imported vectors",
    );
    if (change === "modified") {
      await writeFile(
        join(root, "beacon.md"),
        "# Revised\n\nsilver orchard compass phrase\n",
      );
    } else {
      await rm(join(root, "beacon.md"));
    }
    await index();
    const query = await f.client.callTool({
      name: "zvec_grep_search",
      arguments: {
        root,
        query: "silver orchard compass quiet harbor anchor",
        autoUpdate: false,
        preview: "full",
      },
    });
    assert.notEqual(query.isError, true, JSON.stringify(query));
    const text = JSON.stringify(query.content);
    assert.match(text, /stable.md/);
    assert.match(text, /quiet harbor anchor phrase/);
    assert.doesNotMatch(text, /sealed kernel lantern phrase/);
    if (change === "modified") {
      assert.match(text, /beacon.md/);
      assert.match(text, /silver orchard compass phrase/);
      assert.ok(
        f.model.counts.document > 0,
        "the changed document must be embedded",
      );
    } else {
      assert.doesNotMatch(text, /beacon.md/);
      assert.equal(
        f.model.counts.document,
        0,
        "deletion must not embed the unchanged document",
      );
    }
    const status = await f.client.callTool({
      name: "zvec_grep_index_status",
      arguments: { root },
    });
    assert.equal(
      status.structuredContent.persistent.files.stored,
      change === "deleted" ? 1 : 2,
    );
    assert.equal(
      status.structuredContent.persistent.workspace_index.id,
      f.original.id,
    );
  });
}
