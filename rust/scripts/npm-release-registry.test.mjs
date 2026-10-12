import assert from "node:assert/strict";
import test from "node:test";
import { checkPublishedPackage } from "./npm-release-registry.mjs";

const entry = {
  name: "@zvec/zvec-grep",
  version: "0.0.1",
  integrity: "sha512-verified-tarball",
};

test("allows an unpublished version and an identical partial-release retry", async () => {
  assert.equal(
    await checkPublishedPackage(entry, async () => ({ status: 404 })),
    false,
  );
  assert.equal(
    await checkPublishedPackage(entry, async () => ({
      ok: true,
      json: async () => ({ dist: { integrity: entry.integrity } }),
    })),
    true,
  );
});

test("rejects a version collision before npm publishing", async () => {
  await assert.rejects(
    checkPublishedPackage(entry, async () => ({
      ok: true,
      json: async () => ({ dist: { integrity: "sha512-another-tarball" } }),
    })),
    /already contains different bytes/,
  );
});

test("does not treat a registry outage as an unpublished version", async () => {
  await assert.rejects(
    checkPublishedPackage(entry, async () => ({ status: 503, ok: false })),
    /registry returned 503/,
  );
});
