import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  readdir,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  canonicalFromRelative,
  canonicalRelativePath,
  createCanonicalPathResolver,
  findCanonicalNameCollisions,
  isCanonicalRelativePath,
  makeFileId,
} from "../../dist/engine/utils/canonical-path.js";

// NFC form: U+00E9 (e-acute). NFD form: U+0065 U+0301 (e + combining acute).
const NFC_E = "é";
const NFD_E = "é";

test("canonicalFromRelative normalizes separators, dots and Unicode", () => {
  assert.equal(canonicalFromRelative("a/b/c.md"), "a/b/c.md");
  assert.equal(canonicalFromRelative("./a//b/"), "a/b");
  assert.equal(canonicalFromRelative(`a/caf${NFD_E}.md`), `a/caf${NFC_E}.md`);
});

test("canonicalRelativePath rejects paths outside the workspace", () => {
  const root = join(tmpdir(), "crp-root");
  assert.equal(
    canonicalRelativePath(root, join(root, "views", "one.md")),
    "views/one.md",
  );
  assert.equal(canonicalRelativePath(root, root), null);
  assert.equal(canonicalRelativePath(root, join(root, "..", "other.md")), null);
  assert.equal(canonicalRelativePath(root, "/elsewhere/one.md"), null);
});

test("isCanonicalRelativePath validates form", () => {
  assert.equal(isCanonicalRelativePath("."), true);
  assert.equal(isCanonicalRelativePath("a/b.md"), true);
  assert.equal(isCanonicalRelativePath(""), false);
  assert.equal(isCanonicalRelativePath("/a"), false);
  assert.equal(isCanonicalRelativePath("a/"), false);
  assert.equal(isCanonicalRelativePath("a/../b"), false);
  assert.equal(isCanonicalRelativePath("a//b"), false);
  // Backslash is not a portable separator; it must be rejected outright so
  // stored data cannot change meaning across platforms.
  assert.equal(isCanonicalRelativePath("..\\outside.txt"), false);
  assert.equal(isCanonicalRelativePath("a\\b.md"), false);
});

test("makeFileId depends on the CRP, not the host location", () => {
  const idA = makeFileId("uuid-1", "views/one.md");
  const idB = makeFileId("uuid-1", "views/one.md");
  const idC = makeFileId("uuid-2", "views/one.md");
  const idD = makeFileId("uuid-1", "views/other.md");
  assert.equal(idA, idB);
  assert.notEqual(idA, idC);
  assert.notEqual(idA, idD);
});

test("resolver maps NFD storage through NFC canonical spelling", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-resolve-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "docs"));
  const nfdName = `caf${NFD_E}.md`;
  await writeFile(join(root, "docs", nfdName), "content");
  const resolver = createCanonicalPathResolver(root);
  const crp = canonicalFromRelative(`docs/${nfdName}`);
  assert.equal(crp, `docs/caf${NFC_E}.md`);
  assert.deepEqual(resolver.resolveDetailedSync(crp), {
    status: "ok",
    path: join(root, "docs", nfdName),
  });
  assert.deepEqual(await resolver.resolveDetailed(crp), {
    status: "ok",
    path: join(root, "docs", nfdName),
  });
});

test("resolver reports missing files and resolves the root", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-missing-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const resolver = createCanonicalPathResolver(root);
  assert.deepEqual(resolver.resolveDetailedSync("docs/absent.md"), {
    status: "missing",
  });
  assert.deepEqual(resolver.resolveDetailedSync("."), {
    status: "ok",
    path: resolver.workspaceRoot,
  });
});

test("resolver rejects ambiguous NFC collisions", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-collide-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "docs"));
  await writeFile(join(root, "docs", `caf${NFC_E}.md`), "nfc");
  await writeFile(join(root, "docs", `caf${NFD_E}.md`), "nfd");
  // Record the filesystem's actual entries: normalization-folding
  // filesystems (macOS APFS) collapse the two spellings into one, and
  // ambiguity is only observable where both entries coexist.
  const entries = await readdir(join(root, "docs"));
  const resolver = createCanonicalPathResolver(root);
  if (entries.length === 2) {
    assert.throws(
      () => resolver.resolveDetailedSync(`docs/caf${NFC_E}.md`),
      /ambiguous/,
    );
    return;
  }
  assert.equal(entries.length, 1);
  assert.ok(
    [`caf${NFC_E}.md`, `caf${NFD_E}.md`].includes(entries[0]),
    `unexpected folded entry ${entries[0]}`,
  );
  assert.deepEqual(resolver.resolveDetailedSync(`docs/caf${NFC_E}.md`), {
    status: "ok",
    path: join(root, "docs", entries[0]),
  });
});

test("resolver forbids symlinks escaping the workspace", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-forbid-"));
  const outside = await mkdtemp(join(tmpdir(), "crp-outside-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  t.after(() => rm(outside, { recursive: true, force: true }));
  await writeFile(join(outside, "secret.md"), "outside");
  await symlink(outside, join(root, "linked"));
  const resolver = createCanonicalPathResolver(root);

  const resolution = resolver.resolveDetailedSync("linked/secret.md");
  assert.equal(resolution.status, "forbidden");
  assert.throws(
    () => resolver.requireContainedSync("linked/secret.md"),
    /escapes the workspace/,
  );
  // A missing leaf under an escaping intermediate is also forbidden: the
  // escape is detected at the existing component, never as a missing file.
  const missingInsideEscape = resolver.resolveDetailedSync("linked/absent.md");
  assert.equal(missingInsideEscape.status, "forbidden");
  const asyncMissingInsideEscape =
    await resolver.resolveDetailed("linked/absent.md");
  assert.equal(asyncMissingInsideEscape.status, "forbidden");
  assert.throws(
    () => resolver.requireContainedSync("linked/absent.md"),
    /escapes the workspace/,
  );
});

test("findCanonicalNameCollisions detects Unicode and case groups", () => {
  const collisions = findCanonicalNameCollisions("/x", [
    `caf${NFC_E}.md`,
    `caf${NFD_E}.md`,
    "README.md",
    "readme.md",
    "other.txt",
  ]);
  const unicode = collisions.find((c) => c.kind === "unicode");
  const caseCollision = collisions.find(
    (c) => c.kind === "case" && c.names.includes("README.md"),
  );
  assert.deepEqual(unicode?.names.sort(), [`caf${NFD_E}.md`, `caf${NFC_E}.md`]);
  assert.deepEqual(caseCollision?.names.sort(), ["README.md", "readme.md"]);
});

test("resolver toCanonical round-trips within the workspace", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-roundtrip-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "a"), { recursive: true });
  await writeFile(join(root, "a", "b.md"), "x");
  const resolver = createCanonicalPathResolver(root);
  const crp = resolver.toCanonical(join(root, "a", "b.md"));
  assert.equal(crp, "a/b.md");
  assert.deepEqual(resolver.resolveDetailedSync(crp), {
    status: "ok",
    path: join(root, "a", "b.md"),
  });
  assert.equal(resolver.toCanonical(join(root, "..", "outside.md")), null);
});

test("permission and I/O failures are explicit errors, never missing paths", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-eacces-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "docs"));
  await writeFile(join(root, "docs", "one.md"), "x");
  const { chmod } = await import("node:fs/promises");
  await chmod(join(root, "docs"), 0o000);

  try {
    // Record the platform's actual capability: Windows mode bits are
    // advisory for the owner, so the directory may remain readable.
    let denied = false;
    try {
      createCanonicalPathResolver(root).resolveDetailedSync("docs/one.md");
    } catch (error) {
      assert.match(
        String(error),
        /Filesystem error/,
        "probe failure must be the explicit access error",
      );
      denied = true;
    }

    if (denied) {
      const resolver = createCanonicalPathResolver(root);
      assert.throws(
        () => resolver.resolveDetailedSync("docs/one.md"),
        /Filesystem error/,
        "an unreadable directory is an error, not a missing file",
      );
      assert.throws(
        () => resolver.resolveDetailedSync("docs/absent.md"),
        /Filesystem error/,
        "even a would-be-absent leaf reports the access failure",
      );
      await assert.rejects(
        resolver.resolveDetailed("docs/one.md"),
        /Filesystem error/,
      );
    } else {
      assert.deepEqual(
        createCanonicalPathResolver(root).resolveDetailedSync("docs/one.md"),
        { status: "ok", path: join(root, "docs", "one.md") },
        "advisory mode bits must leave resolution working",
      );
    }
  } finally {
    await chmod(join(root, "docs"), 0o700);
  }

  // Restored access resolves normally again.
  assert.deepEqual(
    createCanonicalPathResolver(root).resolveDetailedSync("docs/one.md"),
    {
      status: "ok",
      path: join(root, "docs", "one.md"),
    },
  );
});

test("contained symlink entries resolve through their link path", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "crp-link-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "real"));
  await writeFile(join(root, "real", "f.md"), "x");
  await symlink(join(root, "real"), join(root, "link"));
  const resolver = createCanonicalPathResolver(root);
  assert.deepEqual(resolver.resolveDetailedSync("link/f.md"), {
    status: "ok",
    path: join(root, "link", "f.md"),
  });
});
