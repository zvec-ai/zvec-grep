"use strict";

const assert = require("node:assert/strict");
const { createHash } = require("node:crypto");
const {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} = require("node:fs");
const { join } = require("node:path");
const { tmpdir } = require("node:os");
const test = require("node:test");

const { materialize, selectPlatform, verifyPayload } = require("./install.cjs");

const entry = {
  target: "darwin-arm64",
  package: "@zvec/zvec-grep-darwin-arm64",
  os: "darwin",
  cpu: "arm64",
  binary: "zg",
};

function sha256(contents) {
  return createHash("sha256").update(contents).digest("hex");
}

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "zvec-grep-npm-install-"));
  const metaRoot = join(root, "meta");
  const platformRoot = join(root, "platform");
  mkdirSync(join(metaRoot, "bin"), { recursive: true });
  mkdirSync(join(platformRoot, "bin"), { recursive: true });
  mkdirSync(join(platformRoot, "lib"), { recursive: true });
  const binary = Buffer.from("#!/bin/sh\necho 0.0.1\n");
  const library = Buffer.from("native-library-fixture\n");
  writeFileSync(join(metaRoot, "bin", "zg.exe"), "placeholder\n");
  writeFileSync(join(platformRoot, "bin", "zg"), binary);
  chmodSync(join(platformRoot, "bin", "zg"), 0o755);
  writeFileSync(join(platformRoot, "lib", "libfixture.dylib"), library);
  writeFileSync(
    join(platformRoot, "package.json"),
    JSON.stringify({ name: entry.package, version: "0.0.1" }),
  );
  writeFileSync(
    join(platformRoot, "checksums.json"),
    JSON.stringify({
      schemaVersion: 1,
      target: entry.target,
      files: {
        "bin/zg": { bytes: binary.length, sha256: sha256(binary) },
        "lib/libfixture.dylib": { bytes: library.length, sha256: sha256(library) },
      },
    }),
  );
  return { root, metaRoot, platformRoot };
}

test("materializes a verified native package and replaces the placeholder last", () => {
  const paths = fixture();
  try {
    materialize({ ...paths, entry, expectedVersion: "0.0.1" });
    assert.match(readFileSync(join(paths.metaRoot, "bin", "zg.exe"), "utf8"), /0\.0\.1/);
    assert.equal(
      readFileSync(join(paths.metaRoot, "lib", "libfixture.dylib"), "utf8"),
      "native-library-fixture\n",
    );
    assert.notEqual(statSync(join(paths.metaRoot, "bin", "zg.exe")).mode & 0o111, 0);
  } finally {
    rmSync(paths.root, { recursive: true, force: true });
  }
});

test("rejects version and checksum mismatches without replacing the placeholder", () => {
  const paths = fixture();
  try {
    assert.throws(
      () => verifyPayload(paths.platformRoot, entry, "0.0.2"),
      /native package mismatch/,
    );
    writeFileSync(join(paths.platformRoot, "bin", "zg"), "corrupt\n");
    assert.throws(
      () => materialize({ ...paths, entry, expectedVersion: "0.0.1" }),
      /checksum mismatch/,
    );
    assert.equal(readFileSync(join(paths.metaRoot, "bin", "zg.exe"), "utf8"), "placeholder\n");
  } finally {
    rmSync(paths.root, { recursive: true, force: true });
  }
});

test("rejects native files that are not covered by the checksum manifest", () => {
  const paths = fixture();
  try {
    writeFileSync(join(paths.platformRoot, "bin", "untracked.dylib"), "untracked\n");
    assert.throws(
      () => verifyPayload(paths.platformRoot, entry, "0.0.1"),
      /do not match the checksum manifest/,
    );
  } finally {
    rmSync(paths.root, { recursive: true, force: true });
  }
});

test("selects one published platform and rejects unsupported libc", () => {
  assert.equal(selectPlatform([entry], "darwin", "arm64"), entry);
  assert.throws(
    () => selectPlatform([entry], "linux", "x64", { glibcVersionRuntime: "2.39" }),
    /does not publish/,
  );
});
