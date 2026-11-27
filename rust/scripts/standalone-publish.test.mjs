import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadReleaseManifest, sha256File } from "./npm-package-core.mjs";
import { publishStandaloneRelease, verifyStandaloneRelease } from "./standalone-publish.mjs";
import { serializeStandaloneManifest } from "./standalone-manifest.mjs";
import { invalidManifestTables } from "./standalone-manifest-fixtures.mjs";

async function fixture(t) {
  const directory = await mkdtemp(join(tmpdir(), "zg-standalone-publish-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const release = { schemaVersion: 1, version: "1.2.3", tag: "v1.2.3", commit: "a".repeat(40), partial: false, assets: [] };
  for (const entry of loadReleaseManifest().platforms) {
    for (const accelerator of entry.components?.length ? ["cpu", "cuda"] : ["cpu"]) {
      const file = `zvec-grep-${release.version}-${entry.target}-${accelerator}.${entry.os === "win32" ? "zip" : "tar.gz"}`;
      await writeFile(join(directory, file), `payload for ${file}`);
      release.assets.push({ target: entry.target, accelerator, file, bytes: (await stat(join(directory, file))).size,
        sha256: await sha256File(join(directory, file)) });
    }
  }
  await writeFile(join(directory, "manifest.tsv"), serializeStandaloneManifest(release));
  for (const file of ["install.sh", "install.ps1"]) await writeFile(join(directory, file), "installer fixture");
  const files = [...release.assets.map((asset) => asset.file), "manifest.tsv", "install.sh", "install.ps1"];
  await writeFile(join(directory, "SHA256SUMS"), (await Promise.all(files.map(async (file) => `${await sha256File(join(directory, file))}  ${file}\n`))).join(""));
  files.push("SHA256SUMS");
  const assets = await Promise.all(files.map(async (file) => ({ name: file, size: (await stat(join(directory, file))).size,
    digest: `sha256:${await sha256File(join(directory, file))}`, state: "uploaded" })));
  return { directory, release, assets, repository: "zvec-ai/zvec-grep", ref: `refs/tags/${release.tag}`, commit: release.commit };
}

test("publication waits for a complete server-verified draft before promoting Latest", async (t) => {
  const data = await fixture(t);
  const calls = [];
  let created = false;
  const gh = (args) => {
    calls.push(args);
    if (args[0] === "api") {
      if (args[1].endsWith("/latest") || args[1].includes("/tags/")) throw new Error("HTTP 404: Not Found");
      if (args[1].includes("?per_page=")) {
        assert.ok(args.includes("--paginate") && args.includes("--slurp"));
        return JSON.stringify([created ? [{ id: 123, tag_name: data.release.tag, draft: true }] : []]);
      }
      assert.ok(created);
      assert.equal(args[1], `repos/${data.repository}/releases/123`);
      return JSON.stringify({ id: 123, draft: true, assets: data.assets });
    }
    if (args[1] === "create") { created = true; assert.equal(args[2], "v1.2.3"); assert.ok(args.includes("--draft")); }
    return "";
  };
  await publishStandaloneRelease({ ...data, gh });
  assert.equal(data.assets.length, 12);
  const upload = calls.find((args) => args[1] === "upload");
  assert.ok(upload.includes(join(data.directory, "manifest.tsv")));
  assert.equal(upload.includes(join(data.directory, "manifest.json")), false);
  assert.deepEqual(calls.filter((args) => args[0] === "release").map((args) => args[1]), ["create", "upload", "edit"]);
  assert.ok(calls.at(-1).includes("--latest=true"));
});

test("draft retries find the exact tag across pages without creating another release", async (t) => {
  const data = await fixture(t);
  const calls = [];
  await publishStandaloneRelease({ ...data, gh: (args) => {
    calls.push(args);
    if (args[0] !== "api") return "";
    if (args[1].includes("/tags/") || args[1].endsWith("/latest")) throw new Error("HTTP 404: Not Found");
    if (args[1].includes("?per_page=")) return JSON.stringify([
      [{ id: 99, tag_name: "v9.9.9", draft: true }],
      [{ id: 123, tag_name: data.release.tag, draft: true }],
    ]);
    assert.equal(args[1], `repos/${data.repository}/releases/123`);
    return JSON.stringify({ id: 123, draft: true, assets: data.assets });
  } });
  assert.deepEqual(calls.filter((args) => args[0] === "release").map((args) => args[1]), ["upload", "edit"]);
});

test("retrying an older draft never demotes Latest and comparison errors fail closed", async (t) => {
  const data = await fixture(t);
  for (const [tag, expected] of [["v1.2.4", false], ["v1.2.2", true], ["v1.2.3", false], ["v1.10.0", false], ["rust-v1.2.4", false]]) {
    const calls = [];
    await publishStandaloneRelease({ ...data, gh: (args) => {
      calls.push(args);
      if (args[0] === "api") return JSON.stringify(args[1].endsWith("/latest") ? { tag_name: tag } : { id: 123, draft: true, assets: data.assets });
      return "";
    } });
    assert.ok(calls.at(-1).includes(`--latest=${expected}`), tag);
  }
  for (const error of [new Error("HTTP 403: denied"), undefined]) {
    const calls = [];
    await assert.rejects(publishStandaloneRelease({ ...data, gh: (args) => {
      calls.push(args);
      if (args[0] === "api" && args[1].endsWith("/latest")) {
        if (error) throw error;
        return JSON.stringify({ tag_name: "unknown" });
      }
      return JSON.stringify({ id: 123, draft: true, assets: data.assets });
    } }));
    assert.equal(calls.some((args) => args[1] === "edit"), false);
  }
});

test("incomplete uploads and unavailable API cannot publish or create an unverified release", async (t) => {
  const data = await fixture(t);
  const calls = [];
  await assert.rejects(publishStandaloneRelease({ ...data, gh: (args) => {
    calls.push(args);
    return JSON.stringify({ id: 123, draft: true, assets: data.assets.slice(1) });
  } }), /digest mismatch/);
  assert.equal(calls.some((args) => args[1] === "edit"), false);
  calls.length = 0;
  await assert.rejects(publishStandaloneRelease({ ...data, gh: (args) => { calls.push(args); throw new Error("HTTP 403: denied"); } }), /403/);
  assert.equal(calls.length, 1);
});

test("published releases are immutable and identical retries never move Latest", async (t) => {
  const data = await fixture(t);
  const calls = [];
  const gh = (args) => { calls.push(args); return JSON.stringify({ draft: false, assets: data.assets }); };
  await publishStandaloneRelease({ ...data, gh });
  assert.equal(calls.length, 1);
  data.assets[0].digest = `sha256:${"0".repeat(64)}`;
  await assert.rejects(publishStandaloneRelease({ ...data, gh }), /digest mismatch/);
  assert.equal(calls.length, 2);
});

test("partial, malformed, corrupt manifests and wrong source identities never reach GitHub", async (t) => {
  const data = await fixture(t);
  const gh = () => assert.fail("invalid input must not reach GitHub");
  await assert.rejects(publishStandaloneRelease({ ...data, repository: "fork/project", gh }), /official repository/);
  await assert.rejects(publishStandaloneRelease({ ...data, ref: "refs/tags/rust-v1.2.3", gh }), /exact verified/);
  await assert.rejects(publishStandaloneRelease({ ...data, commit: "b".repeat(40), gh }), /exact verified/);
  const manifest = join(data.directory, "manifest.tsv");
  const original = await readFile(manifest, "utf8");
  await writeFile(manifest, serializeStandaloneManifest({ ...data.release, partial: true }));
  await assert.rejects(verifyStandaloneRelease(data.directory), /complete tagged stable/);
  for (const tag of ["rust-v1.2.3", "v1.2.4"]) {
    await writeFile(manifest, serializeStandaloneManifest({ ...data.release, tag }));
    await assert.rejects(publishStandaloneRelease({ ...data, gh }), /complete tagged stable/);
  }
  for (const [name, table] of invalidManifestTables(original)) {
    await writeFile(manifest, table);
    await assert.rejects(publishStandaloneRelease({ ...data, gh }), /release manifest/, name);
  }
  await writeFile(manifest, serializeStandaloneManifest({ ...data.release, commit: "b".repeat(40) }));
  await assert.rejects(publishStandaloneRelease({ ...data, gh }), /checksum inventory/);
  await writeFile(manifest, original);
  await writeFile(join(data.directory, data.release.assets[0].file), "tampered payload");
  await assert.rejects(publishStandaloneRelease({ ...data, gh }), /payload mismatch/);
});
