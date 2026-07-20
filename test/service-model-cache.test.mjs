import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { updateGlobalConfig } from "../dist/engine/config.js";
import { EmbeddingModel } from "../dist/engine/models/embeddings.js";
import { createZvecGrep } from "../dist/index.js";
import {
  createRemoteEmbeddingOperationPermit,
  createRemoteEmbeddingTarget,
  withRemoteEmbeddingOperationPermit,
} from "../dist/authorization/index.js";

test("recovered embedding model cache is bounded and defers disposal until active queries finish", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-model-cache-"),
  );
  const root = join(temporaryDirectory, "repo");
  const originalHome = process.env.HOME;
  const originalFetch = globalThis.fetch;
  const originalDispose = EmbeddingModel.prototype.dispose;
  const disposedModels = [];
  let releaseBlockedRequest;
  let service;
  let blockedContext;

  const blockedRequestStarted = new Promise((resolve) => {
    globalThis.fetch = async (input, init) => {
      if (String(input) === "https://blocked.test/embeddings") {
        resolve();
        await new Promise((release) => {
          releaseBlockedRequest = release;
        });
      }

      return embeddingResponse(init);
    };
  });

  process.env.HOME = temporaryDirectory;
  EmbeddingModel.prototype.dispose = async function disposeForTest() {
    disposedModels.push(this);
  };

  try {
    await mkdir(join(root, "src"), { recursive: true });
    await writeFile(
      join(root, "src", "example.ts"),
      "export const answer = 42;\n",
    );
    configureQwen("https://initial.test/embeddings");

    service = await createZvecGrep({
      root,
      embedding: "qwen/text-embedding-v4",
    });
    await withPermit(root, "https://initial.test/embeddings", () =>
      service.index(),
    );

    configureQwen("https://blocked.test/embeddings");
    blockedContext = withPermit(root, "https://blocked.test/embeddings", () =>
      service.context({
        root,
        query: "where is the answer defined",
        limit: 1,
        autoUpdate: false,
      }),
    );
    await blockedRequestStarted;

    for (const name of ["third", "fourth", "fifth", "sixth"]) {
      configureQwen(`https://${name}.test/embeddings`);
      await withPermit(root, `https://${name}.test/embeddings`, () =>
        service.context({
          root,
          query: "where is the answer defined",
          limit: 1,
          autoUpdate: false,
        }),
      );
    }

    assert.equal(disposedModels.length, 0);
    releaseBlockedRequest();
    await blockedContext;
    blockedContext = undefined;
    assert.equal(disposedModels.length, 2);

    await service.close();
    service = undefined;
    assert.equal(disposedModels.length, 6);
  } finally {
    releaseBlockedRequest?.();
    await blockedContext?.catch(() => undefined);
    await service?.close();
    EmbeddingModel.prototype.dispose = originalDispose;
    globalThis.fetch = originalFetch;
    if (originalHome === undefined) {
      delete process.env.HOME;
    } else {
      process.env.HOME = originalHome;
    }
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

function configureQwen(endpoint) {
  updateGlobalConfig({
    providers: {
      qwen: {
        apiKey: "test-key",
        endpoint,
      },
    },
  });
}

async function withPermit(root, endpoint, operation) {
  const target = await createRemoteEmbeddingTarget({
    roots: [root],
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint,
  });
  return await withRemoteEmbeddingOperationPermit(
    createRemoteEmbeddingOperationPermit(target, "once"),
    operation,
  );
}

function embeddingResponse(init) {
  const body = JSON.parse(String(init?.body));
  const contents = Array.isArray(body.input) ? body.input : [];
  return new Response(
    JSON.stringify({
      data: contents.map((_, index) => ({
        index,
        embedding: new Array(1024).fill(0.01),
      })),
    }),
    {
      status: 200,
      headers: { "Content-Type": "application/json" },
    },
  );
}
