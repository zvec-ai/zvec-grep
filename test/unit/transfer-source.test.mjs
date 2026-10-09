import assert from "node:assert/strict";
import { mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

for (const stage of ["copy", "open"]) {
  test(`native transfer source removes its private copy after ${stage} fails`, async (t) => {
    const parent = await mkdtemp(join(tmpdir(), "zg-transfer-source-control-"));
    const keys = ["TMPDIR", "TEMP", "TMP"];
    const previous = new Map(keys.map((key) => [key, process.env[key]]));
    t.after(async () => {
      for (const [key, value] of previous) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
      await rm(parent, { recursive: true, force: true });
    });
    for (const key of keys) process.env[key] = parent;
    assert.equal(tmpdir(), parent, "the fixture must own the actual temp path");
    const source = join(parent, "source");
    await mkdir(join(source, "files.zvec"), { recursive: true });
    await writeFile(join(source, "files.zvec", "keep.txt"), "source remains");
    if (stage === "open") await mkdir(join(source, "index.zvec"));
    const { readNativeTransferSource } =
      await import("../../dist/engine/storage/transfer-source.js");
    assert.throws(
      () => readNativeTransferSource(source),
      stage === "copy" ? /ENOENT/ : /open|collection|manifest|exist/i,
    );
    assert.deepEqual(
      await readdir(parent),
      ["source"],
      "private copies must be removed before test teardown",
    );
    assert.deepEqual(await readdir(join(source, "files.zvec")), ["keep.txt"]);
  });
}
