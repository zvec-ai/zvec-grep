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
import { scanRootPaths } from "../../dist/engine/pipeline/indexing/scanner/index.js";
import { makeFileId } from "../../dist/engine/utils/canonical-path.js";

const NFC_E = "é";
const NFD_E = "é";

async function makeWorkspace(t, prefix = "zg-scan-portable-") {
  const root = await mkdtemp(join(tmpdir(), prefix));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

test("scanned file identity is root-independent within the workspace", async (t) => {
  const root = await makeWorkspace(t);
  await mkdir(join(root, "views", "decisions"), { recursive: true });
  await writeFile(join(root, "views", "decisions", "one.md"), "# One\n");

  const fromWorkspace = await scanRootPaths(
    "index-id",
    [{ absolutePath: root, recursive: true }],
    { workspaceRoot: root },
  );
  const fromSubroot = await scanRootPaths(
    "index-id",
    [{ absolutePath: join(root, "views"), recursive: true }],
    { workspaceRoot: root },
  );

  const idFromWorkspace = fromWorkspace.files.find((file) =>
    file.relativePath.endsWith("one.md"),
  )?.id;
  const idFromSubroot = fromSubroot.files.find((file) =>
    file.relativePath.endsWith("one.md"),
  )?.id;
  assert.ok(idFromWorkspace);
  assert.equal(idFromSubroot, idFromWorkspace);
  assert.equal(
    idFromWorkspace,
    makeFileId("index-id", "views/decisions/one.md"),
  );

  // Legacy mode (no workspaceRoot) keeps absolute-path identities.
  const legacy = await scanRootPaths(
    "index-id",
    [{ absolutePath: root, recursive: true }],
    {},
  );
  assert.notEqual(
    legacy.files.find((file) => file.relativePath.endsWith("one.md"))?.id,
    idFromWorkspace,
  );
});

test("scanner rejects canonical name collisions", async (t) => {
  const root = await makeWorkspace(t);
  await mkdir(join(root, "docs"));
  await writeFile(join(root, "docs", `caf${NFC_E}.md`), "nfc");
  await writeFile(join(root, "docs", `caf${NFD_E}.md`), "nfd");
  // Record the filesystem's actual entries: on normalization-folding
  // filesystems (macOS APFS) the two spellings collapse into one entry
  // and no collision exists to reject; the scan must then succeed with
  // the single NFC-canonical file.
  const entries = await readdir(join(root, "docs"));
  if (entries.length === 2) {
    await assert.rejects(
      () =>
        scanRootPaths("index-id", [{ absolutePath: root, recursive: true }], {
          workspaceRoot: root,
        }),
      (error) =>
        error.code === "ZVEC_GREP.ENGINE.SCANNER.CANONICAL_NAME_COLLISION",
    );
    return;
  }
  assert.equal(entries.length, 1);
  const folded = await scanRootPaths(
    "index-id",
    [{ absolutePath: root, recursive: true }],
    { workspaceRoot: root },
  );
  assert.equal(folded.files.length, 1);
  assert.equal(folded.files[0].canonicalPath, `docs/caf${NFC_E}.md`);
});

test("followed symlinks escaping the workspace are excluded with diagnostics", async (t) => {
  const root = await makeWorkspace(t);
  const outside = await makeWorkspace(t, "zg-scan-outside-");
  await writeFile(join(outside, "secret.md"), "outside");
  await symlink(join(outside, "secret.md"), join(root, "link.md"));

  const result = await scanRootPaths(
    "index-id",
    [{ absolutePath: root, recursive: true, follow: true }],
    { workspaceRoot: root },
  );
  assert.equal(result.files.length, 0);
  assert.equal(result.diagnostics.skippedByReason.escapes_workspace, 1);

  // Without follow the symlink is simply not a file and nothing is reported.
  const noFollow = await scanRootPaths(
    "index-id",
    [{ absolutePath: root, recursive: true }],
    { workspaceRoot: root },
  );
  assert.equal(noFollow.files.length, 0);
  assert.equal(noFollow.diagnostics.skippedByReason.escapes_workspace, 0);
});

test("NFD filenames receive NFC canonical identities and resolve for reading", async (t) => {
  const root = await makeWorkspace(t);
  await mkdir(join(root, "docs"));
  const nfdName = `caf${NFD_E}.md`;
  await writeFile(join(root, "docs", nfdName), "content");

  const result = await scanRootPaths(
    "index-id",
    [{ absolutePath: root, recursive: true }],
    { workspaceRoot: root },
  );
  assert.equal(result.files.length, 1);
  assert.equal(result.files[0].canonicalPath, `docs/caf${NFC_E}.md`);
  assert.equal(
    result.files[0].id,
    makeFileId("index-id", `docs/caf${NFC_E}.md`),
  );
});

test("scan descends into directories under an alias-spelled workspace root", async (t) => {
  // Hosted regression (macOS /var vs /private/var, Windows short names):
  // the caller spells root and workspaceRoot consistently through a parent
  // alias, and the scanner must not compare resolved directories against
  // the unresolved spelling.
  const base = await mkdtemp(join(tmpdir(), "zg-scan-alias-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const physical = join(base, "real", "A");
  await mkdir(join(physical, "docs"), { recursive: true });
  await writeFile(join(physical, "docs", "one.md"), "# One\n");
  await symlink(join(base, "real"), join(base, "var"), "dir");
  const aliasRoot = join(base, "var", "A");

  const result = await scanRootPaths(
    "index-id",
    [{ absolutePath: aliasRoot, recursive: true }],
    { workspaceRoot: aliasRoot },
  );
  assert.equal(result.files.length, 1);
  assert.equal(result.files[0].canonicalPath, "docs/one.md");
  assert.equal(result.files[0].id, makeFileId("index-id", "docs/one.md"));
});

test("followed contained symlinks are scanned under an alias-spelled workspace root", async (t) => {
  const base = await mkdtemp(join(tmpdir(), "zg-scan-alias-follow-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const physical = join(base, "real", "A");
  await mkdir(physical, { recursive: true });
  await writeFile(join(physical, "target.md"), "# Target\n");
  await symlink(join(physical, "target.md"), join(physical, "link.md"));
  await symlink(join(base, "real"), join(base, "var"), "dir");
  const aliasRoot = join(base, "var", "A");

  const result = await scanRootPaths(
    "index-id",
    [{ absolutePath: aliasRoot, recursive: true, follow: true }],
    { workspaceRoot: aliasRoot },
  );
  const followed = result.files.find((file) =>
    file.relativePath.endsWith("link.md"),
  );
  assert.ok(followed, "contained followed symlink must not be skipped");
  assert.equal(followed.canonicalPath, "link.md");
});

test("mixed-spelling scans keep canonical identities in both directions", async (t) => {
  const base = await mkdtemp(join(tmpdir(), "zg-scan-mixed-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const physical = join(base, "real", "A");
  await mkdir(join(physical, "docs"), { recursive: true });
  await writeFile(join(physical, "docs", "one.md"), "# One\n");
  await symlink(join(base, "real"), join(base, "var"), "dir");
  const alias = join(base, "var", "A");

  const rootViaAlias = await scanRootPaths(
    "index-id",
    [{ absolutePath: join(alias, "docs"), recursive: true }],
    { workspaceRoot: physical },
  );
  assert.equal(rootViaAlias.files.length, 1);
  assert.equal(rootViaAlias.files[0].canonicalPath, "docs/one.md");
  assert.equal(rootViaAlias.files[0].id, makeFileId("index-id", "docs/one.md"));

  const workspaceViaAlias = await scanRootPaths(
    "index-id",
    [{ absolutePath: join(physical, "docs"), recursive: true }],
    { workspaceRoot: alias },
  );
  assert.equal(workspaceViaAlias.files.length, 1);
  assert.equal(workspaceViaAlias.files[0].canonicalPath, "docs/one.md");
  assert.equal(
    workspaceViaAlias.files[0].id,
    rootViaAlias.files[0].id,
    "identity must not depend on which side carries the alias spelling",
  );
});
