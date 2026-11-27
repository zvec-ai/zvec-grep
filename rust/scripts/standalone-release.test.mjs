import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { coreIdentity, packageFilename } from "../npm/meta/plugins.cjs";
import { loadReleaseManifest, run, sha256File } from "./npm-package-core.mjs";
import { collectStandaloneRelease } from "./standalone-release.mjs";
import { parseStandaloneManifest, serializeStandaloneManifest } from "./standalone-manifest.mjs";

const checksum = (data) => ({ bytes: Buffer.byteLength(data), sha256: createHash("sha256").update(data).digest("hex") });
const json = (path, value) => writeFile(path, `${JSON.stringify(value)}\n`);

async function fixture(t, target = "linux-x64-gnu") {
  const root = await mkdtemp(join(tmpdir(), "zg-standalone-pack-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const manifest = loadReleaseManifest();
  const entry = manifest.platforms.find((entry) => entry.target === target);
  const version = "0.0.1";
  const release = { schemaVersion: 1, version, commit: "a".repeat(40), targets: [target], partial: true, packages: [] };
  const tarballRoot = join(root, "tarballs");
  const outputDir = join(root, "standalone");
  await mkdir(tarballRoot);
  async function packageArchive(name, files, metadata) {
    const staging = join(root, "packing");
    await rm(staging, { recursive: true, force: true });
    const payload = join(staging, "package");
    await mkdir(payload, { recursive: true });
    for (const [file, data] of Object.entries(files)) {
      await mkdir(dirname(join(payload, file)), { recursive: true });
      await writeFile(join(payload, file), data);
    }
    await json(join(payload, "package.json"), { name, version });
    await json(join(payload, "checksums.json"), metadata);
    const file = packageFilename(name, version);
    run("tar", ["-czf", join(tarballRoot, file), "-C", staging, "package"], { env: { ...process.env, COPYFILE_DISABLE: "1" } });
    const bytes = await readFile(join(tarballRoot, file));
    const pkg = { name, target, version, file, bytes: bytes.length, sha256: checksum(bytes).sha256,
      integrity: `sha512-${createHash("sha512").update(bytes).digest("base64")}` };
    release.packages.push(pkg);
    return pkg;
  }
  const binary = `bin/${entry.binary}`;
  const contents = { [binary]: "fixture binary", "lib/core": "same core", "lib/licenses/upstream.txt": "upstream notice" };
  if (entry.os === "win32") contents["lib/zg-launcher.exe"] = "native launcher";
  const files = Object.fromEntries(Object.entries(contents).map(([name, data]) => [name, checksum(data)]));
  const checksums = { schemaVersion: entry.components?.length ? 3 : 1, target, files };
  if (entry.components?.length) {
    checksums.coreId = coreIdentity(target, version, files);
    const components = [];
    const final = {};
    for (const component of entry.components) {
      const data = `fragment:${component.package}`;
      const payloadChecksum = checksum(data);
      const pkg = await packageArchive(component.package, { [component.payload]: data, "UPSTREAM_NOTICES/provider.txt": "provider notice" },
        { schemaVersion: 1, target, component: component.package, files: { [component.payload]: payloadChecksum } });
      components.push({ ...component, version, tarball: pkg.file, integrity: pkg.integrity, archiveBytes: pkg.bytes, checksum: payloadChecksum });
      final[component.file] = (final[component.file] ?? "") + data;
    }
    const plugins = `${JSON.stringify({ schemaVersion: 1, target, version, coreId: checksums.coreId, components,
      files: Object.fromEntries(Object.entries(final).map(([name, data]) => [name, checksum(data)])) })}\n`;
    contents["plugins.json"] = plugins;
    checksums.pluginsSha256 = checksum(plugins).sha256;
  }
  contents.LICENSE = "project license";
  await packageArchive(entry.package, contents, checksums);
  return { root, manifest, release, tarballRoot, outputDir, entry };
}

test("standalone archives assemble exact CUDA components and preserve base files and notices", async (t) => {
  const data = await fixture(t);
  const inventory = await collectStandaloneRelease(data);
  assert.equal(inventory.partial, true);
  assert.equal(inventory.tag, "v0.0.1");
  const table = await readFile(join(data.outputDir, "manifest.tsv"), "utf8");
  assert.deepEqual(parseStandaloneManifest(table), inventory);
  assert.deepEqual(parseStandaloneManifest(table.replaceAll("\n", "\r\n")), inventory);
  assert.equal(serializeStandaloneManifest(inventory), table);
  assert.ok(table.startsWith(`schema\t1\nversion\t0.0.1\ntag\tv0.0.1\ncommit\t${"a".repeat(40)}\npartial\ttrue\n`));
  assert.equal((await readdir(data.outputDir)).includes("manifest.json"), false);
  const sums = await readFile(join(data.outputDir, "SHA256SUMS"), "utf8");
  assert.ok(sums.includes(`${await sha256File(join(data.outputDir, "manifest.tsv"))}  manifest.tsv\n`));
  assert.doesNotMatch(sums, /manifest\.json/);
  assert.deepEqual(inventory.assets.map((asset) => asset.accelerator), ["cpu", "cuda"]);
  const manifests = [];
  for (const asset of inventory.assets) {
    assert.equal(await sha256File(join(data.outputDir, asset.file)), asset.sha256);
    const destination = join(data.root, asset.accelerator);
    await mkdir(destination);
    run("tar", ["-xzf", join(data.outputDir, asset.file), "-C", destination]);
    const manifest = JSON.parse(await readFile(join(destination, "zg/install-manifest.json"), "utf8"));
    manifests.push(manifest);
    assert.equal(await readFile(join(destination, "zg/LICENSE"), "utf8"), "project license");
    assert.equal(manifest.accelerator, asset.accelerator);
    assert.equal(manifest.requirements.minLibc, "2.28");
    assert.equal(manifest.files["bin/zg.exe"], undefined);
    assert.ok(manifest.files["lib/licenses/upstream.txt"]);
    assert.equal(Object.keys(manifest.files).some((name) => name.includes("ggml-cuda")), asset.accelerator === "cuda");
    assert.equal(Object.keys(manifest.files).some((name) => name.startsWith("parts/")), false);
  }
  assert.deepEqual(manifests[0].files["bin/zg"], manifests[1].files["bin/zg"]);
  assert.deepEqual(manifests[0].files["lib/core"], manifests[1].files["lib/core"]);
  const provider = data.entry.components.filter((component) => component.file.includes("onnxruntime"));
  assert.equal(await readFile(join(data.root, "cuda/zg", provider[0].file), "utf8"), provider.map((component) => `fragment:${component.package}`).join(""));
  assert.ok(Object.keys(manifests[1].files).some((name) => name.includes("UPSTREAM_NOTICES/provider.txt")));
  assert.ok((await readFile(join(data.outputDir, "manifest.tsv"), "utf8")).includes("linux-x64-gnu\tcpu\t"));
});

test("Windows standalone output is a ZIP with native executable layout", async (t) => {
  const data = await fixture(t, "win32-arm64-msvc");
  const inventory = await collectStandaloneRelease(data);
  assert.equal(inventory.assets.length, 1);
  const archive = join(data.outputDir, inventory.assets[0].file);
  assert.equal((await readFile(archive)).subarray(0, 2).toString(), "PK");
  const listing = process.platform === "win32" ? run("tar", ["-tf", archive], { capture: true }) : run("unzip", ["-Z1", archive], { capture: true });
  assert.match(listing, /zg\/bin\/zg.exe/);
  assert.match(listing, /zg\/install-manifest.json/);
  assert.match(listing, /zg\/bin\/zg-launcher.exe/);
});

test("tampered or incomplete collected artifacts cannot become standalone releases", async (t) => {
  const data = await fixture(t);
  const first = join(data.tarballRoot, data.release.packages[0].file);
  await writeFile(first, "corrupt");
  await assert.rejects(collectStandaloneRelease(data), /checksum\/identity mismatch/);
  assert.equal((await readdir(data.outputDir)).includes("manifest.tsv"), false);
  const invalid = { ...data.release, version: "../escape" };
  await assert.rejects(collectStandaloneRelease({ ...data, release: invalid }), /identity/);
  await assert.rejects(collectStandaloneRelease({ ...data, release: { ...data.release, commit: "fixture" } }), /identity/);
});
