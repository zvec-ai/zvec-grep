import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

test("indexed context compacts a class and its methods before applying the global limit", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-overlap-compact-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(
    join(root, "session.ts"),
    [
      "export class SessionNeedleManager {",
      "  SessionNeedlePrimary() {",
      '    return "SessionNeedle primary";',
      "  }",
      "",
      "  SessionNeedleSecondary() {",
      '    return "SessionNeedle secondary";',
      "  }",
      "",
      "  SessionNeedleTertiary() {",
      '    return "SessionNeedle tertiary";',
      "  }",
      "}",
      "",
      "export function SessionNeedleHelper() {",
      '  return "SessionNeedle helper";',
      "}",
      "",
    ].join("\n"),
  );

  const service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(() => service.close());
  await service.index();

  const result = await service.context({
    root,
    routes: [{ mode: "fts", query: "SessionNeedle" }],
    limit: 2,
    trace: true,
    autoUpdate: false,
  });

  assert.equal(result.items.length, 2);
  const family = result.items.find(
    (item) =>
      item.metadata?.kind === "code" &&
      item.metadata.symbolType === "class" &&
      item.metadata.symbolName === "SessionNeedleManager",
  );
  assert.ok(family);
  assert.equal(family.relatedExcerpts?.length, 1);
  assert.ok((family.trace?.compact?.originalRanks.length ?? 0) >= 2);
  assert.ok((family.trace?.compact?.suppressed.length ?? 0) >= 1);
  assert.equal(
    result.items.some(
      (item) =>
        item.metadata?.kind === "code" &&
        item.metadata.scope === "SessionNeedleManager",
    ),
    false,
  );
  assert.ok(
    result.items.some(
      (item) =>
        item.metadata?.kind === "code" &&
        item.metadata.symbolName === "SessionNeedleHelper",
    ),
  );
});
