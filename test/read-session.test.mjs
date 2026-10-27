import assert from "node:assert/strict";
import { mkdir, realpath, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep, openWorkspaceReadSession } from "../dist/index.js";
import { createTemporaryDirectory } from "./helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

test("public read sessions retain a consistent index until closed", async (t) => {
  const directory = await createTemporaryDirectory(
    t,
    "zvec-grep-read-session-",
  );
  const root = join(directory, "repo");
  await mkdir(root);
  const source = join(root, "note.md");
  await writeFile(source, "# Example\n\nAlphaNeedle is here.\n");

  const model = new FakeEmbeddingModel();
  const service = await createZvecGrep({
    root,
    embeddingModel: model,
    embeddingModelOwnership: "borrowed",
  });
  t.after(() => service.close());
  await service.index({ rootPaths: [root], globs: ["*.md"], noIgnore: true });

  const session = openWorkspaceReadSession(root, model);
  t.after(() => session.close());
  assert.equal(session.root, await realpath(root));
  assert.equal((await session.status()).filesIndexed, 1);

  const query = {
    routes: [{ mode: "fts", query: "AlphaNeedle" }],
    autoUpdate: false,
  };
  const first = await session.context(query);
  assert.ok(first.items.some((item) => item.content.includes("AlphaNeedle")));

  await writeFile(source, "# Example\n\nBetaNeedle is here.\n");
  await assert.rejects(service.index(), (error) =>
    error.code?.includes("LOCK"),
  );

  const pending = session.context(query);
  const closing = session.close();
  assert.ok((await pending).items.length > 0);
  await closing;
  await session.close();
  await assert.rejects(session.status(), (error) =>
    error.code?.endsWith("READ_SESSION_CLOSED"),
  );
  await assert.rejects(session.context(query), (error) =>
    error.code?.endsWith("READ_SESSION_CLOSED"),
  );

  await service.index();
  const next = openWorkspaceReadSession(root, model);
  try {
    const result = await next.context({
      routes: [{ mode: "fts", query: "BetaNeedle" }],
      autoUpdate: false,
    });
    assert.ok(result.items.some((item) => item.content.includes("BetaNeedle")));
  } finally {
    await next.close();
  }
});
