import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import test from "node:test";

test("checkout Transformers.js loads patched sharp and decodes a benign image", async () => {
  const runtimeUrl = import.meta.resolve("@huggingface/transformers");
  const runtimeRequire = createRequire(runtimeUrl);
  const { default: sharp } = await import(
    pathToFileURL(runtimeRequire.resolve("sharp"))
  );
  const [major, minor, patch] = sharp.versions.sharp.split(".").map(Number);
  assert.ok(
    major > 0 || minor > 35 || (minor === 35 && patch >= 4),
    `Transformers.js resolved vulnerable sharp ${sharp.versions.sharp}`,
  );

  const runtime = await import(runtimeUrl);
  const bytes = await sharp({
    create: { width: 2, height: 1, channels: 3, background: "white" },
  })
    .png()
    .toBuffer();
  const image = await runtime.RawImage.fromBlob(new Blob([bytes]));
  assert.deepEqual(image.size, [2, 1]);
  assert.equal(image.channels, 3);
});
