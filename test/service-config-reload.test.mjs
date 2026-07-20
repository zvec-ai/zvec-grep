import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { updateGlobalConfig } from "../dist/engine/config.js";
import { createZvecGrep } from "../dist/index.js";
import {
  createRemoteEmbeddingOperationPermit,
  createRemoteEmbeddingTarget,
  withRemoteEmbeddingOperationPermit,
} from "../dist/authorization/index.js";

test("explicit embedding reference reloads provider config before refresh and query", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-reload-"),
  );
  const root = join(temporaryDirectory, "repo");
  const originalHome = process.env.HOME;
  const originalFetch = globalThis.fetch;
  const requests = [];
  let service;

  process.env.HOME = temporaryDirectory;
  globalThis.fetch = async (input, init) => {
    const body = JSON.parse(String(init?.body));
    const contents = Array.isArray(body.input) ? body.input : [];
    requests.push({
      url: String(input),
      authorization: init?.headers?.Authorization,
    });
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
  };

  try {
    await mkdir(join(root, "src"), { recursive: true });
    await writeFile(
      join(root, "src", "example.ts"),
      "export const answer = 42;\n",
    );
    updateGlobalConfig({
      providers: {
        qwen: {
          apiKey: "key-a",
          endpoint: "https://endpoint-a.test/embeddings",
        },
      },
    });

    service = await createZvecGrep({
      root,
      embedding: "qwen/text-embedding-v4",
    });
    await withPermit(root, "https://endpoint-a.test/embeddings", () =>
      service.index(),
    );
    assert.ok(requests.length > 0);
    assert.ok(
      requests.every(
        (request) => request.url === "https://endpoint-a.test/embeddings",
      ),
    );
    assert.ok(
      requests.every((request) => request.authorization === "Bearer key-a"),
    );

    requests.length = 0;
    updateGlobalConfig({
      providers: {
        qwen: {
          apiKey: "key-b",
          endpoint: "https://endpoint-b.test/embeddings",
        },
      },
    });
    await writeFile(
      join(root, "src", "example.ts"),
      "export const answer = 43;\n",
    );

    await withPermit(root, "https://endpoint-b.test/embeddings", () =>
      service.context({
        root,
        query: "where is the answer defined",
        limit: 1,
      }),
    );
    assert.ok(requests.length > 0);
    assert.ok(
      requests.every(
        (request) => request.url === "https://endpoint-b.test/embeddings",
      ),
    );
    assert.ok(
      requests.every((request) => request.authorization === "Bearer key-b"),
    );
  } finally {
    await service?.close();
    globalThis.fetch = originalFetch;
    if (originalHome === undefined) {
      delete process.env.HOME;
    } else {
      process.env.HOME = originalHome;
    }
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

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
