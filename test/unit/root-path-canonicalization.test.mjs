import assert from "node:assert/strict";
import { realpathSync } from "node:fs";
import { mkdir, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createCanonicalPathResolver } from "../../dist/engine/utils/canonical-path.js";
import { scanRootPaths } from "../../dist/engine/pipeline/indexing/scanner/index.js";
import {
  manifestRootPathsFromRuntime,
  validateRootPaths,
} from "../../dist/engine/pipeline/indexing/root-paths.js";
import { workspaceIndexLocation } from "../../dist/engine/service/root.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

// Hosted macOS regression (2026-09-30 portability preflight): os.tmpdir()
// addresses the workspace as /var/folders/... while workspaceIndexLocation
// resolves the existing workspace home to /private/var/folders/... The
// runtime root and the resolver's workspace root must canonicalize against
// equivalent physical roots, never textual spellings alone.

async function prepareSymlinkedWorkspace(t) {
  const base = await createTemporaryDirectory(t, "zvec-root-canon-");
  const physicalRoot = join(base, "real", "A");
  await mkdir(join(physicalRoot, ".zvec-grep"), { recursive: true });
  // Stand-in for /var -> /private/var: a parent alias of the physical tree.
  await symlink(join(base, "real"), join(base, "var"));
  return { base, physicalRoot, symlinkedRoot: join(base, "var", "A") };
}

test("workspace root through a symlinked parent canonicalizes to the workspace root CRP", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);

  const location = workspaceIndexLocation(symlinkedRoot);
  assert.equal(location.root, realpathSync(physicalRoot));

  const resolver = createCanonicalPathResolver(location.root);
  const manifestRoots = manifestRootPathsFromRuntime(
    validateRootPaths([symlinkedRoot]),
    resolver,
  );

  assert.equal(manifestRoots.length, 1);
  assert.equal(manifestRoots[0].path, ".");
  assert.equal(manifestRoots[0].recursive, true);
});

test("child root through a symlinked parent keeps its workspace-relative CRP", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  await mkdir(join(physicalRoot, "views"));

  const location = workspaceIndexLocation(symlinkedRoot);
  const resolver = createCanonicalPathResolver(location.root);
  const manifestRoots = manifestRootPathsFromRuntime(
    validateRootPaths([join(symlinkedRoot, "views")]),
    resolver,
  );

  assert.equal(manifestRoots[0].path, "views");
});

test("resolved runtime root against an unresolved workspace root canonicalizes consistently", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);

  const resolver = createCanonicalPathResolver(symlinkedRoot);
  const manifestRoots = manifestRootPathsFromRuntime(
    validateRootPaths([physicalRoot]),
    resolver,
  );

  assert.equal(manifestRoots[0].path, ".");
});

test("roots escaping the workspace through symlinks stay rejected", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  const outside = await createTemporaryDirectory(t, "zvec-root-canon-out-");
  await writeFile(join(outside, "secret.md"), "outside\n");
  await symlink(outside, join(physicalRoot, "escape"));

  const resolver = createCanonicalPathResolver(
    workspaceIndexLocation(symlinkedRoot).root,
  );

  assert.throws(
    () =>
      manifestRootPathsFromRuntime(
        validateRootPaths([join(symlinkedRoot, "escape")]),
        resolver,
      ),
    { code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_OUTSIDE_WORKSPACE" },
  );
  assert.throws(
    () => manifestRootPathsFromRuntime(validateRootPaths([outside]), resolver),
    { code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_OUTSIDE_WORKSPACE" },
  );
});

test("a missing leaf beneath an escaping intermediate stays an explicit error", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  const outside = await createTemporaryDirectory(t, "zvec-root-canon-out-");
  await symlink(outside, join(physicalRoot, "escape"));

  assert.throws(
    () => validateRootPaths([join(symlinkedRoot, "escape", "missing.md")]),
    { code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_MISSING" },
  );
});

test("equivalent root spellings persist the logical name and identical file identities", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  await mkdir(join(physicalRoot, "target"));
  await writeFile(join(physicalRoot, "target", "file.md"), "# Portable\n");
  await symlink(
    join(physicalRoot, "target"),
    join(physicalRoot, "link"),
    "dir",
  );

  const resolver = createCanonicalPathResolver(physicalRoot);
  const fileIds = [];
  for (const spelling of [physicalRoot, symlinkedRoot]) {
    const manifestRoot = manifestRootPathsFromRuntime(
      validateRootPaths([join(spelling, "link")]),
      resolver,
    )[0];
    assert.equal(manifestRoot.path, "link");

    const resolution = resolver.resolveDetailedSync(manifestRoot.path);
    assert.equal(resolution.status, "ok");

    const scan = await scanRootPaths(
      "same-index-id",
      [
        {
          absolutePath: resolution.path,
          canonicalPath: manifestRoot.path,
          recursive: true,
        },
      ],
      { workspaceRoot: physicalRoot },
    );
    assert.equal(scan.files.length, 1);
    assert.equal(scan.files[0].canonicalPath, "link/file.md");
    fileIds.push(scan.files[0].id);
  }
  assert.equal(fileIds[0], fileIds[1]);
});

test("internal workspace-root link selected under both spellings persists the link name", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  await mkdir(join(physicalRoot, "docs"), { recursive: true });
  await writeFile(join(physicalRoot, "docs", "file.md"), "# Portable\n");
  await symlink(physicalRoot, join(physicalRoot, "self"), "dir");

  const resolver = createCanonicalPathResolver(physicalRoot);
  const fileIds = [];
  for (const spelling of [physicalRoot, symlinkedRoot]) {
    const manifestRoot = manifestRootPathsFromRuntime(
      validateRootPaths([join(spelling, "self")]),
      resolver,
    )[0];
    assert.equal(manifestRoot.path, "self");

    const resolution = resolver.resolveDetailedSync(manifestRoot.path);
    assert.equal(resolution.status, "ok");

    const scan = await scanRootPaths(
      "same-index-id",
      [
        {
          absolutePath: resolution.path,
          canonicalPath: manifestRoot.path,
          recursive: true,
        },
      ],
      { workspaceRoot: physicalRoot },
    );
    const files = scan.files
      .map((file) => ({ path: file.canonicalPath, id: file.id }))
      .sort((left, right) => left.path.localeCompare(right.path));
    assert.deepEqual(
      files.map((file) => file.path),
      ["self/docs/file.md"],
    );
    fileIds.push(files[0].id);
  }
  assert.equal(fileIds[0], fileIds[1]);
});

test("directory below the internal workspace-root link preserves the full logical path", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  await mkdir(join(physicalRoot, "docs"), { recursive: true });
  await writeFile(join(physicalRoot, "docs", "file.md"), "# Portable\n");
  await symlink(physicalRoot, join(physicalRoot, "self"), "dir");

  const resolver = createCanonicalPathResolver(physicalRoot);
  const fileIds = [];
  for (const spelling of [physicalRoot, symlinkedRoot]) {
    const manifestRoot = manifestRootPathsFromRuntime(
      validateRootPaths([join(spelling, "self", "docs")]),
      resolver,
    )[0];
    assert.equal(manifestRoot.path, "self/docs");

    const resolution = resolver.resolveDetailedSync(manifestRoot.path);
    assert.equal(resolution.status, "ok");

    const scan = await scanRootPaths(
      "same-index-id",
      [
        {
          absolutePath: resolution.path,
          canonicalPath: manifestRoot.path,
          recursive: true,
        },
      ],
      { workspaceRoot: physicalRoot },
    );
    assert.equal(scan.files.length, 1);
    assert.equal(scan.files[0].canonicalPath, "self/docs/file.md");
    fileIds.push(scan.files[0].id);
  }
  assert.equal(fileIds[0], fileIds[1]);
});

test("ignore-file entries resolve through workspace alias spellings", async (t) => {
  const { physicalRoot, symlinkedRoot } = await prepareSymlinkedWorkspace(t);
  await mkdir(join(physicalRoot, "docs"));
  await writeFile(join(physicalRoot, "docs", ".ignore"), "ignored\n");

  const resolver = createCanonicalPathResolver(physicalRoot);
  const manifestRoot = manifestRootPathsFromRuntime(
    validateRootPaths([
      {
        absolutePath: physicalRoot,
        recursive: true,
        ignoreFiles: [join(symlinkedRoot, "docs", ".ignore")],
      },
    ]),
    resolver,
  )[0];
  assert.deepEqual(manifestRoot.ignoreFiles, ["docs/.ignore"]);

  // Relative entries joined onto an alias-spelled root behave the same.
  const relativeEntry = manifestRootPathsFromRuntime(
    validateRootPaths([
      {
        absolutePath: symlinkedRoot,
        recursive: true,
        ignoreFiles: ["docs/.ignore"],
      },
    ]),
    resolver,
  )[0];
  assert.deepEqual(relativeEntry.ignoreFiles, ["docs/.ignore"]);
});
