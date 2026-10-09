import assert from "node:assert/strict";
import { Worker } from "node:worker_threads";
import { createHash } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";

useIsolatedZvecGrepHome();

async function hashes(root) {
  const result = {};
  async function visit(directory, prefix = "") {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      // The operation must create and release its own lock. It must not
      // change any source manifest, collection, vector or native storage byte.
      if (prefix === "" && entry.name === "locks") continue;
      const name = prefix + entry.name;
      if (entry.isDirectory())
        await visit(join(directory, entry.name), name + "/");
      else
        result[name] = createHash("sha256")
          .update(await readFile(join(directory, entry.name)))
          .digest("hex");
    }
  }
  await visit(root);
  return result;
}

for (const operation of ["migration", "export"]) {
  test(`${operation} preserves every source native-storage byte in a native worker`, async (t) => {
    const parent = await mkdtemp(
      join(tmpdir(), "zg-migrate-source-preservation-"),
    );
    t.after(() => rm(parent, { recursive: true, force: true }));
    const root = join(parent, "source");
    const destinationRoot = join(parent, "destination");
    const content = "# Beacon\n\nsealed kernel lantern phrase\n";
    for (const directory of [root, destinationRoot]) {
      await mkdir(directory);
      await writeFile(join(directory, "beacon.md"), content);
      await writeFile(
        join(directory, "stable.md"),
        "# Anchor\n\nquiet harbor anchor phrase\n",
      );
    }
    const service = await createZvecGrep({
      root,
      embeddingModel: new FakeEmbeddingModel(),
    });
    try {
      await service.index();
    } finally {
      await service.close();
    }
    const manifest = JSON.parse(
      await readFile(join(root, ".zvec-grep/manifest.json"), "utf8"),
    );
    const sourceHome = join(root, ".zvec-grep-legacy");
    await buildLegacyHome(root, sourceHome, manifest.id);
    const before = await hashes(sourceHome);
    assert.ok(
      Object.keys(before).some((name) => name.endsWith(".proxima")),
      "the check must include the native vector index",
    );
    const modulePath = operation === "migration" ? "migrate" : "transfer";
    const functionName =
      operation === "migration"
        ? "migrateWorkspaceIndex"
        : "exportWorkspaceIndex";
    const moduleUrl = new URL(
      `../../dist/engine/${modulePath}/index.js`,
      import.meta.url,
    ).href;
    const script = `const { parentPort, workerData } = require("node:worker_threads");
      import(workerData.moduleUrl).then(async (module) => {
        parentPort.postMessage(await module[workerData.functionName](workerData.options));
      });`;
    const result = await new Promise((resolve, reject) => {
      const worker = new Worker(script, {
        eval: true,
        workerData: {
          moduleUrl,
          functionName,
          options: {
            sourceHome,
            destinationRoot,
            artifactPath: join(parent, "artifact"),
            verifySampleLimit: 0,
          },
        },
      });
      let output;
      worker.on("message", (value) => {
        output = value;
      });
      worker.on("error", reject);
      worker.on("exit", (code) => {
        if (code === 0 && output) resolve(output);
        else reject(new Error(`native worker exit ${code} without result`));
      });
    });
    assert.equal(result.indexId, manifest.id);
    if (operation === "migration") {
      assert.equal(result.verification.vectorsExact, true);
      assert.equal(result.verification.vectorsSampled, false);
    } else {
      assert.equal(result.filesExported, 2);
      assert.ok(result.entitiesExported > 0);
    }
    assert.deepEqual(await hashes(sourceHome), before);
    const locks = await readdir(join(sourceHome, "locks"), { recursive: true });
    assert.ok(
      !locks.some((name) => name.endsWith("lock.json")),
      "the operation must release its source lock",
    );
  });
}
