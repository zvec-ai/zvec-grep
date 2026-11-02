import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { verifyPackageCache } from "./rust-package-cache.mjs";

export const NODE_RELEASE = Object.freeze({
  identity: "npm:@zvec/zvec-grep@0.2.2",
  url: "https://registry.npmjs.org/@zvec/zvec-grep/-/zvec-grep-0.2.2.tgz",
  sha256: "a958bb24f161c6d092fe555ddeeb44989f933354d9e21392d89af4bc75bd4393",
  integrity:
    "sha512-6xsF21zgUh98W3BmH7+Nz/Esdx4Ps3ibs+DHluh/uo2O45xCMeM+pkmdRlcagkONHRO2pM2KiGgKHVmRGKxBtA==",
});

function verifyNodeBytes(contents) {
  assert.equal(
    createHash("sha256").update(contents).digest("hex"),
    NODE_RELEASE.sha256,
    "Node.js 0.2.2 package differs from the pinned official release",
  );
  assert.equal(
    `sha512-${createHash("sha512").update(contents).digest("base64")}`,
    NODE_RELEASE.integrity,
    "Node.js 0.2.2 registry integrity mismatch",
  );
}

export async function downloadNodePackage({ output, fetcher = fetch }) {
  const response = await fetcher(NODE_RELEASE.url);
  assert.ok(response.ok, `Node.js package download failed: ${response.status}`);
  const contents = Buffer.from(await response.arrayBuffer());
  verifyNodeBytes(contents);
  output = resolve(output);
  await mkdir(output, { recursive: true });
  await writeFile(join(output, "candidate.tgz"), contents);
  const manifest = {
    schema_version: 1,
    runtime: "node-0.2.2",
    candidate_commit: NODE_RELEASE.identity,
    tarball: "candidate.tgz",
    tarball_sha256: NODE_RELEASE.sha256,
    registry_url: NODE_RELEASE.url,
    registry_integrity: NODE_RELEASE.integrity,
  };
  await writeFile(
    join(output, "manifest.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
  );
  return manifest;
}

export async function verifyRetrievalPackage({ directory, commit }) {
  if (commit !== NODE_RELEASE.identity)
    return verifyPackageCache({ directory, commit });
  directory = resolve(directory);
  assert.deepEqual((await readdir(directory)).sort(), [
    "candidate.tgz",
    "manifest.json",
  ]);
  const manifest = JSON.parse(
    await readFile(join(directory, "manifest.json"), "utf8"),
  );
  assert.equal(manifest.schema_version, 1);
  assert.equal(manifest.runtime, "node-0.2.2");
  assert.equal(manifest.candidate_commit, NODE_RELEASE.identity);
  assert.equal(manifest.tarball, "candidate.tgz");
  assert.equal(manifest.tarball_sha256, NODE_RELEASE.sha256);
  assert.equal(manifest.registry_url, NODE_RELEASE.url);
  assert.equal(manifest.registry_integrity, NODE_RELEASE.integrity);
  verifyNodeBytes(await readFile(join(directory, "candidate.tgz")));
  return manifest;
}

async function main(args) {
  const command = args.shift();
  const { values } = parseArgs({
    args,
    options: Object.fromEntries(
      ["output", "directory", "commit"].map((key) => [key, { type: "string" }]),
    ),
  });
  const result =
    command === "download-node"
      ? await downloadNodePackage({ output: values.output })
      : command === "verify"
        ? await verifyRetrievalPackage(values)
        : assert.fail("usage: retrieval-package.mjs <download-node|verify>");
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  await main(process.argv.slice(2));
