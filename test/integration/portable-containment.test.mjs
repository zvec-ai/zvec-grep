import assert from "node:assert/strict";
import { cp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
import {
  assertDenialInducible,
  probeDenial,
} from "../helpers/permission-probe.mjs";

useIsolatedZvecGrepHome();

const DOCS = { "docs/guide.md": "# Guide\n\nContained content marker.\n" };

async function writeFiles(root, files) {
  for (const [relative, content] of Object.entries(files)) {
    const target = join(root, relative);
    await mkdir(dirname(target), { recursive: true });
    await writeFile(target, content);
  }
}

test("a saved root replaced by an escaping symlink is rejected on indexing", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-contain-root-");
  const C = join(parent, "C");
  const D = join(parent, "D");
  const outside = join(parent, "outside");
  await mkdir(C, { recursive: true });
  await mkdir(outside, { recursive: true });
  await writeFiles(C, DOCS);
  await writeFile(
    join(outside, "external.md"),
    "# External\n\nOutside marker.\n",
  );

  const serviceC = await createZvecGrep({
    root: C,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await serviceC.index({
    rootPaths: [{ absolutePath: join(C, "docs"), recursive: true }],
  });
  await serviceC.close();

  await cp(C, D, { recursive: true });
  await rm(join(D, "docs"), { recursive: true, force: true });
  await symlink(outside, join(D, "docs"));

  const serviceD = await createZvecGrep({
    root: D,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    () => serviceD.index(),
    (error) =>
      /outside the workspace|escapes the workspace/i.test(error.message),
    "indexing must reject an escaping saved root",
  );
  await assert.rejects(
    () => serviceD.context({ query: "marker", limit: 3 }),
    (error) =>
      /outside the workspace|escapes the workspace/i.test(error.message),
    "search must not resolve an escaping saved root either",
  );
  await serviceD.close();
});

test("search does not follow a directory swapped for an escaping symlink", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-contain-read-");
  const root = join(parent, "W");
  const outside = join(parent, "outside");
  await mkdir(root, { recursive: true });
  await mkdir(outside, { recursive: true });
  await writeFiles(root, DOCS);
  // The impostor shares the indexed file's name: serving it through the
  // swapped tree must be impossible.
  await writeFile(
    join(outside, "guide.md"),
    "# Guide\n\nExternal impostor content.\n",
  );

  const model = new FakeEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  await service.close();

  // Swap the indexed directory for an escaping symlink after indexing.
  await rm(join(root, "docs"), { recursive: true, force: true });
  await symlink(outside, join(root, "docs"));

  const service2 = await createZvecGrep({ root, embeddingModel: model });
  await assert.rejects(
    () =>
      service2.context({
        query: "contained content marker",
        limit: 3,
        autoUpdate: false,
      }),
    (error) => /escapes the workspace/i.test(error.message),
    "stored records inside an escaping tree must raise a containment error",
  );
  await service2.close();

  // Restoring the real directory restores normal reads.
  await rm(join(root, "docs"), { force: true });
  await writeFiles(root, DOCS);
  const service3 = await createZvecGrep({ root, embeddingModel: model });
  const result = await service3.context({
    query: "contained content marker",
    limit: 3,
    autoUpdate: false,
  });
  assert.ok(result.items.length > 0);
  await service3.close();
});

test("storage and search fail loudly under permission errors, then recover", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-contain-eacces-");
  const root = join(parent, "W");
  await mkdir(root, { recursive: true });
  await writeFiles(root, DOCS);

  const model = new FakeEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  await service.close();

  const { chmod, readdir } = await import("node:fs/promises");
  await chmod(join(root, "docs"), 0o000);
  try {
    // Whether mode bits deny the owner's read is a platform capability,
    // measured independently of the code under test.
    const denied =
      (await probeDenial(t, "containment.query:readdir-docs", () =>
        readdir(join(root, "docs")),
      )) === "denied";
    t.diagnostic(
      `permission-branch operation=containment.query branch=${
        denied ? "strict" : "advisory"
      }`,
    );
    assertDenialInducible(
      t,
      "containment.query",
      denied ? "denied" : "allowed",
    );
    const service2 = await createZvecGrep({ root, embeddingModel: model });
    if (denied) {
      await assert.rejects(
        () =>
          service2.context({
            query: "contained content marker",
            limit: 3,
            autoUpdate: false,
          }),
        /Filesystem error/,
        "an unreadable indexed directory must surface as an error, not silence",
      );
    } else {
      // Advisory mode bits (Windows): the query must keep working.
      const advisory = await service2.context({
        query: "contained content marker",
        limit: 3,
        autoUpdate: false,
      });
      assert.ok(advisory.items.length > 0);
    }
    await service2.close();
  } finally {
    await chmod(join(root, "docs"), 0o700);
  }

  const service3 = await createZvecGrep({ root, embeddingModel: model });
  const result = await service3.context({
    query: "contained content marker",
    limit: 3,
    autoUpdate: false,
  });
  assert.ok(result.items.length > 0);
  await service3.close();
});

test("a changed parent symlink is caught when refreshing a copied workspace", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-contain-refresh-");
  const E = join(parent, "E");
  const F = join(parent, "F");
  const outside = join(parent, "outside");
  await mkdir(E, { recursive: true });
  await mkdir(outside, { recursive: true });
  await writeFiles(E, DOCS);
  await writeFile(
    join(outside, "external.md"),
    "# External\n\nOutside marker.\n",
  );

  const serviceE = await createZvecGrep({
    root: E,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await serviceE.index();
  await serviceE.close();
  await cp(E, F, { recursive: true });

  // F's indexed directory is swapped for an escaping symlink whose target
  // holds an impostor with the same file name, before any refresh.
  await rm(join(F, "docs"), { recursive: true, force: true });
  await writeFile(
    join(outside, "guide.md"),
    "# Guide\n\nExternal impostor content.\n",
  );
  await symlink(outside, join(F, "docs"));

  const serviceF = await createZvecGrep({
    root: F,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    () => serviceF.context({ query: "marker", limit: 3 }),
    (error) =>
      /outside the workspace|escapes the workspace/i.test(error.message),
    "refresh must refuse to index outside content through the swapped tree",
  );
  await serviceF.close();
});
