import assert from "node:assert/strict";
import {
  readdir,
  cp,
  mkdir,
  readFile,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { ZVecInitialize, ZVecLogLevel } from "@zvec/zvec";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../../dist/engine/transfer/index.js";
import { createZvecGrep } from "../../dist/index.js";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { restoreIndexedMtime } from "../helpers/mtime.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";

useIsolatedZvecGrepHome();

const NFC_E = "é";
const NFD_E = "é";

async function indexFixture(root, content = "alpha indexed document") {
  await mkdir(join(root, "docs"), { recursive: true });
  await writeFile(join(root, "docs", "one.md"), `${content}\n`);
}

async function sameStatReplace(target, from, to) {
  const indexedStat = await stat(target);
  const original = await readFile(target, "utf8");
  const replaced = original.replace(from, to);
  assert.equal(replaced.length, original.length);
  await writeFile(target, replaced);
  await restoreIndexedMtime(target, indexedStat.mtimeMs);
}

test("a supported same-UUID replacement invalidates verification", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-bind-replace-");
  const A = join(parent, "A");
  const B = join(parent, "B");
  await indexFixture(A, "alpha indexed document");
  await indexFixture(B, "alpha indexed document");

  // Export a closed index of A, import into B, and verify B by indexing.
  const serviceA = await createZvecGrep({
    root: A,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await serviceA.index();
  await serviceA.close();
  const artifact = join(parent, "artifact");
  await exportWorkspaceIndex({
    sourceHome: join(A, ".zvec-grep"),
    artifactPath: artifact,
  });
  await importWorkspaceIndex({ artifactPath: artifact, destinationRoot: B });
  const modelB = new CountingEmbeddingModel();
  const serviceB = await createZvecGrep({ root: B, embeddingModel: modelB });
  await serviceB.index(); // establishes B's binding
  assert.equal(modelB.counts.document, 0);

  // Same-stat edit at B, then replace only B's .zvec-grep with another
  // import of the ORIGINAL artifact (same index UUID, same workspace).
  await sameStatReplace(join(B, "docs", "one.md"), "alpha", "omega");
  const artifact2 = join(parent, "artifact2");
  await rename(artifact, artifact2);
  await exportWorkspaceIndex({
    sourceHome: join(A, ".zvec-grep"),
    artifactPath: artifact,
  });
  await rm(join(B, ".zvec-grep"), { recursive: true, force: true });
  await importWorkspaceIndex({ artifactPath: artifact2, destinationRoot: B });

  // The replaced storage must not be trusted: the next index reconciles and
  // finds the same-stat edit; only the changed document is embedded.
  const modelB2 = new CountingEmbeddingModel();
  const serviceB2 = await createZvecGrep({ root: B, embeddingModel: modelB2 });
  const result = await serviceB2.index();
  assert.equal(
    result.filesModified,
    1,
    "the supported replacement must force reconciliation of the changed file",
  );
  assert.equal(modelB2.counts.document, 1);
  const search = await serviceB2.context({ query: "omega indexed", limit: 3 });
  assert.ok(search.items.length > 0);
  await serviceB2.close();
});

test("unsupported in-place restore stays trusted until forced reconciliation", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-bind-manual-");
  const root = join(parent, "W");
  await indexFixture(root, "alpha indexed document");

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  await service.close();

  // Manual, unsupported restore: swap collection files back inside the
  // existing storage directory (its directory inode is preserved).
  const home = join(root, ".zvec-grep");
  const stash = join(parent, "stash");
  await cp(home, stash, { recursive: true });
  await sameStatReplace(join(root, "docs", "one.md"), "alpha", "omega");
  await cp(join(stash, "files.zvec"), join(home, "files.zvec"), {
    recursive: true,
  });
  await cp(join(stash, "index.zvec"), join(home, "index.zvec"), {
    recursive: true,
  });

  // Documented limitation: without the forced path, the unchanged binding
  // keeps the pre-restore trust and the same-stat edit is not detected.
  const service2 = await createZvecGrep({
    root,
    embeddingModel: new CountingEmbeddingModel(),
  });
  const trusted = await service2.index();
  assert.equal(trusted.filesModified, 0);

  // The documented forced path reconciles and detects the edit.
  const model3 = new CountingEmbeddingModel();
  const service3 = await createZvecGrep({ root, embeddingModel: model3 });
  const reconciled = await service3.index({ reconcile: true });
  assert.equal(reconciled.filesModified, 1);
  assert.equal(model3.counts.document, 1);
  await service3.close();
  await service2.close();
});

test("cancelled forced reconciliation leaves the index unverified for retry", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-bind-cancel-");
  const root = join(parent, "W");
  await indexFixture(root, "alpha indexed document");

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  await service.close();

  await sameStatReplace(join(root, "docs", "one.md"), "alpha", "omega");

  // Start a forced reconciliation and cancel it after scanning begins.
  const controller = new AbortController();
  const cancelling = new CountingEmbeddingModel();
  const service2 = await createZvecGrep({
    root,
    embeddingModel: cancelling,
  });
  await assert.rejects(
    service2.index({
      reconcile: true,
      signal: controller.signal,
      onProgress: () => controller.abort(),
    }),
    undefined,
    "the forced reconciliation is cancelled",
  );
  await service2.close();

  // The prior verification was dropped when the forced run started, so the
  // ordinary retry reconciles and detects the same-stat edit.
  const model3 = new CountingEmbeddingModel();
  const service3 = await createZvecGrep({ root, embeddingModel: model3 });
  const result = await service3.index();
  assert.equal(
    result.filesModified,
    1,
    "the ordinary retry must reconcile after a cancelled forced run",
  );
  assert.equal(model3.counts.document, 1);
  const search = await service3.context({ query: "omega indexed", limit: 3 });
  assert.ok(search.items.length > 0);
  await service3.close();
});

test("invalidation propagates access failures and never silently keeps trust", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-bind-eacces-");
  const root = join(parent, "W");
  await indexFixture(root, "alpha indexed document");

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  await service.close();

  const { readWorkspaceManifest } =
    await import("../../dist/engine/manifest.js");
  const { WorkspaceBindingStore } =
    await import("../../dist/engine/bindings.js");
  const manifest = readWorkspaceManifest(join(root, ".zvec-grep"));
  const store = new WorkspaceBindingStore();
  assert.equal(store.matches(manifest.id, root), true);

  // With the workspace unreadable, invalidation must fail loudly instead of
  // reporting a revocation that never happened. Whether mode bits deny the
  // owner's read is a platform capability, measured independently here;
  // unexpected probe errors propagate rather than passing as advisory.
  const { chmod } = await import("node:fs/promises");
  const { probeDenial, assertDenialInducible } =
    await import("../helpers/permission-probe.mjs");
  await chmod(parent, 0o000);
  try {
    const denied =
      (await probeDenial(t, "bindings.invalidate:readdir-parent", () =>
        readdir(parent),
      )) === "denied";
    t.diagnostic(
      `permission-branch operation=bindings.invalidate branch=${
        denied ? "strict" : "advisory"
      }`,
    );
    assertDenialInducible(
      t,
      "bindings.invalidate",
      denied ? "denied" : "allowed",
    );
    if (denied) {
      assert.throws(
        () => store.invalidate(manifest.id, root),
        /EACCES|permission denied/i,
        "an access failure during invalidation must propagate",
      );
    } else {
      // Advisory mode bits (Windows): nothing is denied, so invalidation
      // must keep working rather than fail.
      store.invalidate(manifest.id, root);
    }
  } finally {
    await chmod(parent, 0o755);
  }

  // Restored access: invalidation works, and verification is really gone.
  store.invalidate(manifest.id, root);
  assert.equal(store.matches(manifest.id, root), false);
});

test("binding updates are serialized and never resurrect an invalidation", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-bind-serialize-");
  const root = join(parent, "W");
  await indexFixture(root, "alpha indexed document");

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  await service.close();

  const { WorkspaceBindingStore } =
    await import("../../dist/engine/bindings.js");
  const manifest = await import("../../dist/engine/manifest.js");
  const info = manifest.readWorkspaceManifest(join(root, ".zvec-grep"));
  const store = new WorkspaceBindingStore();

  // Parallel record/invalidate cycles on the same index UUID must leave a
  // readable, consistent record — never a torn or resurrected state.
  await Promise.all(
    Array.from({ length: 12 }, (_, index) =>
      index % 2 === 0
        ? Promise.resolve(store.record(info.id, root))
        : Promise.resolve(store.invalidate(info.id, root)),
    ),
  );
  store.invalidate(info.id, root);
  assert.equal(
    store.matches(info.id, root),
    false,
    "the final invalidation stands; no earlier snapshot can resurrect it",
  );
  store.record(info.id, root);
  assert.equal(store.matches(info.id, root), true);
});
ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
test("distinct NFC and NFD workspace roots never share a binding", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-bind-unicode-");
  const nfcRoot = join(parent, `workspac${NFC_E}`);
  const nfdRoot = join(parent, `workspac${NFD_E}`);
  await indexFixture(nfcRoot, "alpha indexed document");
  await indexFixture(nfdRoot, "alpha indexed document");

  const modelNfc = new CountingEmbeddingModel();
  const serviceNfc = await createZvecGrep({
    root: nfcRoot,
    embeddingModel: modelNfc,
  });
  await serviceNfc.index();
  await serviceNfc.close();

  // The siblings are physically different directories — except on
  // normalization-folding filesystems (macOS APFS), where the two
  // spellings collapse into one entry and "distinct roots" cannot exist.
  // Record that capability: the binding-separation requirement below is
  // only testable where the siblings are distinct.
  if ((await stat(nfdRoot)).ino === (await stat(nfcRoot)).ino) {
    return;
  }
  assert.notEqual((await stat(nfdRoot)).ino, (await stat(nfcRoot)).ino);

  // Copy the verified index (same UUID) to the NFD sibling and change one
  // file there with identical stat. The NFC binding must not leak: the NFD
  // root is inspected as itself, so its first index reconciles.
  await cp(join(nfcRoot, ".zvec-grep"), join(nfdRoot, ".zvec-grep"), {
    recursive: true,
  });
  await sameStatReplace(join(nfdRoot, "docs", "one.md"), "alpha", "omega");
  const modelNfd = new CountingEmbeddingModel();
  const serviceNfd = await createZvecGrep({
    root: nfdRoot,
    embeddingModel: modelNfd,
  });
  const result = await serviceNfd.index();
  assert.equal(
    result.filesModified,
    1,
    "the NFD sibling must reconcile on its own binding, not inherit NFC's",
  );
  assert.equal(modelNfd.counts.document, 1);
  await serviceNfd.close();
});
