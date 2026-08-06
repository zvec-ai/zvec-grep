import assert from "node:assert/strict";
import {
  link,
  mkdir,
  readFile,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { hostname } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { detectFileType } from "../../dist/engine/file-type.js";
import {
  fileBelongsToRootPath,
  matchesRootExcludePatterns,
  matchesRootIncludePatterns,
  matchesRootPatterns,
  normalizeRootPath,
  validateRootPaths,
} from "../../dist/engine/pipeline/indexing/root-paths.js";
import { scanRootPaths } from "../../dist/engine/pipeline/indexing/scanner/index.js";
import {
  readJsonFile,
  readJsonFileSync,
  writeJsonFile,
  writeJsonFileSync,
} from "../../dist/engine/utils/json.js";
import {
  acquireReadWriteLock,
  assertNoWriteLock,
} from "../../dist/engine/utils/lock.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("file detection covers special, code, data, text, image, binary, and unknown files", () => {
  assert.deepEqual(detectFileType("Dockerfile"), {
    kind: "code",
    format: "dockerfile",
  });
  assert.deepEqual(detectFileType("Makefile"), {
    kind: "code",
    format: "makefile",
  });
  assert.deepEqual(detectFileType("A.TS"), {
    kind: "code",
    format: "typescript",
  });
  assert.deepEqual(detectFileType("data.jsonc"), {
    kind: "data",
    format: "json",
  });
  assert.deepEqual(detectFileType("README.mdx"), {
    kind: "text",
    format: "markdown",
  });
  assert.deepEqual(detectFileType("photo.JPG"), {
    kind: "image",
    format: "jpeg",
  });
  assert.equal(detectFileType("archive.zip"), null);
  assert.deepEqual(detectFileType("custom.xyz"), {
    kind: "text",
    format: "xyz",
  });
  assert.deepEqual(detectFileType("NOTICE"), {
    kind: "text",
    format: "text",
  });
});

test("root path validation detects missing and overlapping scan domains", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-root-paths-");
  const child = join(root, "child");
  const file = join(root, "direct.ts");
  const otherFile = join(root, "other.ts");
  const hardLink = join(root, "direct-link.ts");
  await mkdir(child);
  await writeFile(file, "export {};\n");
  await writeFile(otherFile, "export const other = true;\n");
  await link(file, hardLink);

  assert.deepEqual(normalizeRootPath(root), {
    absolutePath: root,
    recursive: true,
  });
  assert.throws(
    () => validateRootPaths([join(root, "missing")]),
    /does not exist/,
  );
  assert.throws(() => validateRootPaths([root, child]), /overlap/);
  assert.throws(() => validateRootPaths([root, file]), /overlap/);
  assert.throws(() => validateRootPaths([file, hardLink]), /overlap/);
  assert.equal(validateRootPaths([file, otherFile]).length, 2);
  assert.throws(
    () => validateRootPaths([{ absolutePath: root, recursive: false }, file]),
    /overlap/,
  );
  assert.equal(
    validateRootPaths([file, { absolutePath: child, recursive: false }]).length,
    2,
  );
  assert.equal(
    validateRootPaths([
      { absolutePath: root, recursive: false },
      { absolutePath: child, recursive: true },
    ]).length,
    2,
  );

  const filtered = {
    absolutePath: root,
    recursive: true,
    include: ["src/**", "README.md"],
    exclude: ["**/*.test.ts"],
  };
  assert.equal(matchesRootPatterns("src/main.ts", filtered), true);
  assert.equal(matchesRootPatterns("src/main.test.ts", filtered), false);
  assert.equal(matchesRootPatterns("other.txt", filtered), false);
  assert.equal(matchesRootIncludePatterns("README.md", filtered), true);
  assert.equal(matchesRootExcludePatterns("src/a.test.ts", filtered), true);
  assert.equal(
    fileBelongsToRootPath(join(root, "src/main.ts"), filtered),
    true,
  );
  assert.equal(fileBelongsToRootPath(join(root, "other.txt"), filtered), false);
  assert.equal(
    fileBelongsToRootPath(join(root, "..", "outside.ts"), filtered),
    false,
  );
});

test("scanner applies ignore files, hidden and generated directories, size, binary, and path filters", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-scanner-");
  await mkdir(join(root, "src", "nested"), { recursive: true });
  await mkdir(join(root, "node_modules", "pkg"), { recursive: true });
  await mkdir(join(root, "dist"), { recursive: true });
  await mkdir(join(root, ".hidden"), { recursive: true });
  await mkdir(join(root, "vendor"), { recursive: true });
  await mkdir(join(root, "src", "nested", ".git"), { recursive: true });
  await writeFile(
    join(root, ".gitignore"),
    "ignored.txt\nvendor/\n!vendor/keep.ts\n",
  );
  await writeFile(join(root, "src", "main.ts"), "export const main = 1;\n");
  await writeFile(join(root, "src", "skip.log"), "skip\n");
  await writeFile(join(root, "ignored.txt"), "ignored\n");
  await writeFile(join(root, "vendor", "keep.ts"), "export {};\n");
  await writeFile(join(root, "node_modules", "pkg", "index.js"), "ignored\n");
  await writeFile(join(root, "dist", "output.js"), "ignored\n");
  await writeFile(join(root, ".hidden", "secret.ts"), "hidden\n");
  await writeFile(join(root, "src", "nested", "child.ts"), "nested\n");
  await writeFile(join(root, "binary.unknown"), Buffer.from([0, 1, 2, 0, 3]));
  await writeFile(join(root, "large.txt"), "x".repeat(1024 * 1024 + 1));

  const result = await scanRootPaths("collection", [
    {
      absolutePath: root,
      recursive: true,
      include: ["src/**", "vendor/keep.ts"],
      exclude: ["**/*.log"],
    },
  ]);
  const paths = result.files.map((item) => item.relativePath).sort();
  assert.deepEqual(paths, [
    "src/main.ts",
    "src/nested/child.ts",
    "vendor/keep.ts",
  ]);
  assert.equal(result.files[0].id.length, 64);

  const single = await scanRootPaths("collection", [
    { absolutePath: join(root, "src", "main.ts"), recursive: false },
  ]);
  assert.equal(single.files.length, 1);
  assert.equal(single.files[0].absolutePath, join(root, "src", "main.ts"));
  assert.equal(single.files[0].relativePath, "main.ts");

  const implicit = await scanRootPaths("collection", [root]);
  assert.equal(
    implicit.files.some((item) => item.relativePath === "src/nested/child.ts"),
    false,
  );
});

test("scanner excludes general low-signal content and permits explicit includes", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-scanner-defaults-");
  await mkdir(join(root, "src"), { recursive: true });
  await mkdir(join(root, "docs"), { recursive: true });
  await mkdir(join(root, "fixtures"), { recursive: true });
  await mkdir(join(root, "locales"), { recursive: true });
  await mkdir(join(root, ".github", "workflows"), { recursive: true });
  await mkdir(join(root, ".hidden"), { recursive: true });

  const retainedFiles = {
    "src/main.ts": "export const main = true;\n",
    "docs/guide.md": "# Guide\n",
    "fixtures/input.json": '{"input": true}\n',
  };
  const ignoredFiles = {
    "locales/en.json": '{"hello": "Hello"}\n',
    "messages.po": 'msgid "Hello"\nmsgstr "Hello"\n',
    "messages.pot": 'msgid "Hello"\nmsgstr ""\n',
    "package-lock.json": '{"lockfileVersion": 3}\n',
    "yarn.lock": "package@1.0.0:\n",
    "Cargo.lock": "[[package]]\n",
    "Package.resolved": '{"pins": []}\n',
    "app.min.js": "function a(){return 1}\n",
    "app.bundle.mjs": "export default {};\n",
    "app.js.map": '{"version": 3}\n',
    "api.generated.ts": "export const generated = true;\n",
    "client.pb.go": "package client\n",
    "service_pb2.py": "GENERATED = True\n",
    "view.g.ts": "export const generated = true;\n",
    "logo.png": "not-a-real-image",
    ".github/workflows/ci.yml": "name: CI\n",
    ".hidden/secret.ts": "export const secret = true;\n",
    ".editorconfig": "root = true\n",
    ".eslintrc.json": '{"root": true}\n',
    ".env": "TOKEN=secret\n",
    ".env.example": "TOKEN=replace-me\n",
  };

  for (const [path, content] of Object.entries({
    ...retainedFiles,
    ...ignoredFiles,
  })) {
    await writeFile(join(root, path), content);
  }

  const result = await scanRootPaths("collection", [root]);
  assert.deepEqual(
    result.files.map((file) => file.relativePath).sort(),
    Object.keys(retainedFiles).sort(),
  );

  const explicitlyIncluded = await scanRootPaths("collection", [
    {
      absolutePath: root,
      recursive: true,
      include: [
        "locales/en.json",
        "package-lock.json",
        "client.pb.go",
        "logo.png",
      ],
    },
  ]);
  assert.deepEqual(
    explicitlyIncluded.files.map((file) => file.relativePath).sort(),
    ["client.pb.go", "locales/en.json", "logo.png", "package-lock.json"],
  );
});

test("scanner applies rg-style globs, types, discovery controls, and safe symlink following", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-scanner-rg-options-");
  await mkdir(join(root, "src", "deep"), { recursive: true });
  await mkdir(join(root, ".hidden"), { recursive: true });
  await writeFile(join(root, ".gitignore"), "ignored.ts\n");
  await writeFile(join(root, "root.ts"), "export const root = 1;\n");
  await writeFile(join(root, "ignored.ts"), "export const ignored = 1;\n");
  await writeFile(join(root, "root.py"), "root = 1\n");
  await writeFile(join(root, "skip.test.ts"), "export const skip = 1;\n");
  await writeFile(
    join(root, ".hidden", "secret.ts"),
    "export const secret = 1;\n",
  );
  await writeFile(join(root, "src", "child.ts"), "export const child = 1;\n");
  await writeFile(
    join(root, "src", "deep", "grand.ts"),
    "export const grand = 1;\n",
  );
  if (process.platform !== "win32") {
    await symlink(join(root, "root.ts"), join(root, "linked.ts"));
  }

  const result = await scanRootPaths("collection", [
    {
      absolutePath: root,
      recursive: true,
      globs: ["**", "!**/*.test.ts"],
      fileTypes: ["ts"],
      hidden: true,
      noIgnore: true,
      maxDepth: 2,
      maxFileSizeBytes: 1024,
      follow: true,
    },
  ]);
  assert.deepEqual(result.files.map((file) => file.relativePath).sort(), [
    ".hidden/secret.ts",
    "ignored.ts",
    ...(process.platform === "win32" ? [] : ["linked.ts"]),
    "root.ts",
    "src/child.ts",
  ]);

  const rootOnly = await scanRootPaths("collection", [
    {
      absolutePath: root,
      recursive: true,
      globs: ["!*.ts", "root.ts"],
      fileTypes: ["ts"],
      noIgnore: true,
      maxDepth: 1,
    },
  ]);
  assert.deepEqual(
    rootOnly.files.map((file) => file.relativePath),
    ["root.ts"],
  );

  const depthZero = await scanRootPaths("collection", [
    {
      absolutePath: root,
      recursive: true,
      noIgnore: true,
      maxDepth: 0,
    },
  ]);
  assert.equal(depthZero.files.length, 0);
});

test("JSON helpers provide fallbacks, atomic replacement, modes, and parse failures", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-json-");
  const asyncPath = join(root, "async", "value.json");
  const syncPath = join(root, "sync", "value.json");
  assert.deepEqual(await readJsonFile(asyncPath, { fallback: true }), {
    fallback: true,
  });
  assert.deepEqual(readJsonFileSync(syncPath, ["fallback"]), ["fallback"]);

  await writeJsonFile(
    asyncPath,
    { value: 1 },
    { directoryMode: 0o700, fileMode: 0o600 },
  );
  writeJsonFileSync(
    syncPath,
    { value: 2 },
    { directoryMode: 0o700, fileMode: 0o600 },
  );
  assert.deepEqual(await readJsonFile(asyncPath, null), { value: 1 });
  assert.deepEqual(readJsonFileSync(syncPath, null), { value: 2 });
  assert.match(await readFile(asyncPath, "utf8"), /\n$/);
  if (process.platform !== "win32") {
    assert.equal((await stat(asyncPath)).mode & 0o777, 0o600);
    assert.equal((await stat(join(root, "async"))).mode & 0o777, 0o700);
  }

  await writeFile(asyncPath, "not-json");
  await assert.rejects(readJsonFile(asyncPath, null), SyntaxError);
  assert.throws(() => readJsonFileSync(asyncPath, null), SyntaxError);
});

test("read/write locks allow readers, reject conflicts, release, and recover stale owners", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-lock-");
  const lockPath = join(root, "index.lock");
  const readOne = acquireReadWriteLock(lockPath, "read", {
    operation: "search-1",
  });
  const readTwo = acquireReadWriteLock(lockPath, "read", {
    operation: "search-2",
  });
  assert.throws(
    () => acquireReadWriteLock(lockPath, "write", { operation: "index" }),
    /Index unavailable/,
  );
  readOne.release();
  readOne.release();
  readTwo.release();

  const write = acquireReadWriteLock(lockPath, "write", { operation: "index" });
  assert.throws(
    () => assertNoWriteLock(lockPath, "status"),
    /Index unavailable/,
  );
  assert.throws(
    () => acquireReadWriteLock(lockPath, "read", { operation: "search" }),
    /Index unavailable/,
  );
  write.release();
  assert.doesNotThrow(() => assertNoWriteLock(lockPath, "status"));

  const stalePath = `${lockPath}.write`;
  await mkdir(stalePath, { recursive: true });
  await writeFile(
    join(stalePath, "lock.json"),
    JSON.stringify({
      token: "stale",
      pid: 999_999_999,
      hostname: hostname(),
      startedAt: 0,
      operation: "stale",
    }),
  );
  const recovered = acquireReadWriteLock(lockPath, "write", {
    operation: "recovered",
    staleMs: 1,
  });
  assert.equal(recovered.info.operation, "recovered");
  recovered.release();
});
