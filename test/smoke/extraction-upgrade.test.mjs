import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import test from "node:test";
import { ZVecOpen } from "@zvec/zvec";
import { createZvecGrep, createEmbeddingModel } from "../../dist/index.js";
import {
  extractForIndexing,
  currentCodeExtractionVersion,
} from "../../dist/engine/extraction/index.js";
import { createWorkspaceIndexStorage } from "../../dist/engine/storage/index.js";
import { resolveWorkspaceIndexStoragePaths } from "../../dist/engine/storage/layout.js";
import { DaemonClient } from "../../dist/client/daemon-client.js";
import { readInstanceRecord } from "../../dist/daemon/server-controller.js";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";

test(
  "ordinary CLI search implicitly upgrades a legacy code index with the actual local model",
  {
    skip: !process.env.ZVEC_GREP_MODEL_CACHE,
  },
  async (t) => {
    const temporary = await createTemporaryDirectory(
      t,
      "zvec-default-upgrade-",
      { cleanup: false },
    );
    const root = join(temporary, "repo");
    const home = join(temporary, "home");
    await mkdir(root);
    const path = join(root, "config.ts");
    await writeFile(
      path,
      'const settings = { description: "network backoff policy", attempts: 3 };\nexport function echo(value) { return value; }\n',
    );
    const probe = createServer();
    await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
    const port = probe.address().port;
    await new Promise((resolve) => probe.close(resolve));
    const serverUrl = `http://127.0.0.1:${port}/mcp`;
    const reference = "local/potion-code-16m-v2";
    const env = {
      HOME: home,
      USERPROFILE: home,
      ZVEC_GREP_HOME: home,
      NO_COLOR: "1",
      ZVEC_GREP_SERVER_URL: serverUrl,
      ZVEC_GREP_EMBEDDING: reference,
      ZVEC_GREP_MODEL_CACHE: process.env.ZVEC_GREP_MODEL_CACHE,
    };
    t.after(async () => {
      await runCli(["--server", "off"], { cwd: root, env }).catch(
        () => undefined,
      );
      await removeTemporaryDirectory(temporary);
    });
    const service = await createZvecGrep({
      root,
      embedding: reference,
      modelCacheDir: env.ZVEC_GREP_MODEL_CACHE,
    });
    let info;
    try {
      await service.index();
      info = await service.info();
    } finally {
      await service.close();
    }
    const storage = createWorkspaceIndexStorage({
      storagePath: info.home,
      readOnly: false,
      embedding: info.workspaceIndex.embedding,
    });
    const model = createEmbeddingModel(reference, {
      modelCacheDir: env.ZVEC_GREP_MODEL_CACHE,
    });
    try {
      const file = storage.getFileByPath(path);
      const text = await readFile(path, "utf8");
      const fragments = (await extractForIndexing({ kind: "text", text, file }))
        .map(({ fragment }) => fragment)
        .filter((fragment) => fragment.metadata);
      const { vectors } = await model.embed(
        fragments.map((fragment) => fragment.content),
      );
      storage.replaceFile(
        file,
        fragments.map((fragment, index) => ({
          fragment,
          vector: vectors[index],
        })),
      );
    } finally {
      storage.close();
      await model.dispose();
    }
    const legacyReader = createWorkspaceIndexStorage({
      storagePath: info.home,
      readOnly: true,
    });
    try {
      assert.deepEqual(legacyReader.searchFts("network backoff policy", 5), []);
      const legacyFile = legacyReader.getFileByPath(path);
      assert.ok(
        legacyReader
          .listEntitiesByFile(legacyFile.id)
          .every(({ entity }) => entity.range.startLine > 1),
        "the old index must omit the configuration declaration",
      );
    } finally {
      legacyReader.close();
    }
    const native = ZVecOpen(
      resolveWorkspaceIndexStoragePaths(info.home).filesPath,
      { readOnly: false },
    );
    try {
      native.dropColumnSync("extraction_version");
    } finally {
      native.closeSync();
    }

    await runCli(["network backoff policy"], { cwd: root, env });
    const daemon = await readInstanceRecord(home);
    assert.ok(daemon?.ready);
    const client = new DaemonClient({ serverUrl, home });
    let status = await client.callTool("zvec_grep_index_status", { root });
    assert.ok(
      status.runtime?.active_job_id,
      "ordinary search must schedule the upgrade without --index or --refresh",
    );
    const deadline = performance.now() + 20_000;
    while (
      status.runtime.job_state !== "succeeded" &&
      performance.now() < deadline
    ) {
      assert.ok(!["failed", "cancelled"].includes(status.runtime.job_state));
      await new Promise((resolve) => setTimeout(resolve, 200));
      status = await client.callTool("zvec_grep_index_status", { root });
    }
    assert.equal(status.runtime.job_state, "succeeded");
    assert.equal(status.persistent.files.modified, 0);
    const result = await runCli(["network backoff policy"], { cwd: root, env });
    assert.match(result.stdout, /^#1 .*config\.ts/m);
    assert.match(result.stdout, /network backoff policy/);
    assert.equal((await readInstanceRecord(home)).pid, daemon.pid);
    const current = createWorkspaceIndexStorage({
      storagePath: info.home,
      readOnly: true,
    });
    try {
      assert.equal(
        current.listFiles()[0].indexStatus.extractionVersion,
        currentCodeExtractionVersion("typescript"),
      );
    } finally {
      current.close();
    }
  },
);
