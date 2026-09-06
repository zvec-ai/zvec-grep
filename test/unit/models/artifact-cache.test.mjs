import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdir,
  open,
  readFile,
  readdir,
  stat,
  utimes,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { resolveModelArtifacts } from "../../../dist/engine/models/artifact-downloader.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

const bytes = Buffer.from("verified cached model artifact");
const artifact = {
  path: "onnx/model.onnx",
  size: bytes.byteLength,
  sha256: createHash("sha256").update(bytes).digest("hex"),
};

async function cachedSnapshot(t) {
  const directory = await createTemporaryDirectory(t, "zvec-artifact-cache-");
  const destination = join(directory, artifact.path);
  await mkdir(join(directory, "onnx"));
  await writeFile(destination, bytes);
  const network = { calls: 0 };
  const options = {
    model: "local/cache-test-model",
    sources: [
      {
        kind: "huggingface",
        repo: "owner/model",
        revision: "pinned-revision",
        cacheDirectory: directory,
      },
    ],
    artifacts: [artifact],
    dependencies: {
      async fetch() {
        network.calls++;
        throw new Error("a valid cache must not access the network");
      },
    },
  };
  await resolveModelArtifacts(options);
  const markerNames = (await readdir(directory)).filter(
    (name) =>
      name.startsWith(".zvec-grep-artifacts-") && name.endsWith(".complete"),
  );
  assert.equal(markerNames.length, 1);
  const markerPath = join(directory, markerNames[0]);
  const marker = JSON.parse(await readFile(markerPath, "utf8"));
  assert.equal(network.calls, 0);
  return { directory, destination, markerPath, marker, options, network };
}

test("repairs malformed completion metadata without downloading healthy cached files", async (t) => {
  const cache = await cachedSnapshot(t);
  const invalidMarkers = [
    ["null marker", null],
    ["primitive marker", "not a marker"],
    ["array marker", []],
    ["null files", { ...cache.marker, files: null }],
    ["array files", { ...cache.marker, files: [] }],
    ["null file entry", { ...cache.marker, files: { [artifact.path]: null } }],
    [
      "primitive file entry",
      { ...cache.marker, files: { [artifact.path]: 1 } },
    ],
    ["array file entry", { ...cache.marker, files: { [artifact.path]: [] } }],
    [
      "invalid file metadata",
      {
        ...cache.marker,
        files: {
          [artifact.path]: {
            ...cache.marker.files[artifact.path],
            size: String(artifact.size),
          },
        },
      },
    ],
  ];

  for (const [name, invalidMarker] of invalidMarkers) {
    await t.test(name, async () => {
      await writeFile(cache.markerPath, JSON.stringify(invalidMarker));

      const result = await resolveModelArtifacts(cache.options);

      assert.equal(result.paths[artifact.path], cache.destination);
      assert.deepEqual(await readFile(cache.destination), bytes);
      assert.equal(cache.network.calls, 0);
      assert.deepEqual(
        JSON.parse(await readFile(cache.markerPath, "utf8")),
        cache.marker,
      );
    });
  }
});

test("replaces same-size corruption with verified bytes without mutating an open predecessor", async (t) => {
  const cache = await cachedSnapshot(t);
  const corruptedBytes = Buffer.alloc(bytes.byteLength, 120);
  await writeFile(cache.destination, corruptedBytes);
  const originalStats = await stat(cache.destination);
  // Make the marker stale even on filesystems with coarse timestamp precision.
  await utimes(
    cache.destination,
    originalStats.atime,
    new Date(cache.marker.files[artifact.path].mtimeMs + 2_000),
  );
  const predecessor = await open(cache.destination, "r");
  let downloads = 0;
  try {
    const result = await resolveModelArtifacts({
      ...cache.options,
      dependencies: {
        async fetch() {
          downloads++;
          return new Response(bytes);
        },
      },
    });

    assert.equal(downloads, 1);
    assert.equal(result.paths[artifact.path], cache.destination);
    assert.deepEqual(await readFile(cache.destination), bytes);
    assert.deepEqual(await predecessor.readFile(), corruptedBytes);

    // A repaired snapshot must be reusable with the network unavailable.
    await resolveModelArtifacts(cache.options);
    assert.equal(cache.network.calls, 0);
    const names = await readdir(cache.directory, { recursive: true });
    assert.deepEqual(
      names.filter(
        (name) => name.includes(".part-") || name.includes(".replaced-"),
      ),
      [],
    );
  } finally {
    await predecessor.close();
  }
});
