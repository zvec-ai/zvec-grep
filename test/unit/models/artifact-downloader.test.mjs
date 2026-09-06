import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  ArtifactDownloadError,
  ModelArtifactResolutionError,
  isFallbackEligibleArtifactError,
  modelArtifactUrl,
  resolveModelArtifacts,
} from "../../../dist/engine/models/artifact-downloader.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

const bytes = Buffer.from("verified model artifact");
const artifact = Object.freeze({
  path: "onnx/model q4.onnx",
  size: bytes.byteLength,
  sha256: createHash("sha256").update(bytes).digest("hex"),
});

function sources(root, overrides = {}) {
  return [
    {
      kind: "huggingface",
      repo: "owner/model",
      revision: "hf-revision",
      cacheDirectory: join(root, "huggingface"),
      ...overrides.huggingface,
    },
    {
      kind: "modelscope",
      repo: "iic/model",
      revision: "ms-revision",
      cacheDirectory: join(root, "modelscope"),
      ...overrides.modelscope,
    },
  ];
}

function options(root, overrides = {}) {
  return {
    model: "local/test-model",
    sources: sources(root),
    artifacts: [artifact],
    lock: { pollMs: 2, staleMs: 1_000, heartbeatMs: 100 },
    ...overrides,
  };
}

test("builds encoded Hugging Face and ModelScope artifact URLs", () => {
  assert.equal(
    modelArtifactUrl(
      {
        kind: "huggingface",
        repo: "owner/model name",
        revision: "release/one",
      },
      "onnx/model q4.onnx",
    ),
    "https://huggingface.co/owner/model%20name/resolve/release%2Fone/onnx/model%20q4.onnx",
  );
  assert.equal(
    modelArtifactUrl(
      {
        kind: "modelscope",
        repo: "iic/model",
        revision: "abc123",
      },
      "model.safetensors",
    ),
    "https://modelscope.cn/models/iic/model/resolve/abc123/model.safetensors",
  );
});

test("checks every cache before networking and returns a mapped ModelScope snapshot", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-cache-");
  const mapped = "snapshot/model.onnx";
  const modelScopeDirectory = join(root, "modelscope");
  await mkdir(join(modelScopeDirectory, "snapshot"), { recursive: true });
  await writeFile(join(modelScopeDirectory, mapped), bytes);
  let fetchCalls = 0;

  const result = await resolveModelArtifacts(
    options(root, {
      // Passing sources backwards also verifies the fixed HF -> MS ordering.
      sources: sources(root, {
        modelscope: { localPaths: { [artifact.path]: mapped } },
      }).reverse(),
      onDownloadPlan() {
        assert.fail("a complete cache must not start a download plan");
      },
      dependencies: {
        async fetch() {
          fetchCalls++;
          throw new Error("network must not be used");
        },
      },
    }),
  );

  assert.equal(fetchCalls, 0);
  assert.equal(result.source.kind, "modelscope");
  assert.equal(result.directory, modelScopeDirectory);
  assert.equal(result.paths[artifact.path], join(modelScopeDirectory, mapped));
  assert.deepEqual(await readFile(result.paths[artifact.path]), bytes);
  assert.ok(
    (await readdir(modelScopeDirectory)).some(
      (name) =>
        name.startsWith(".zvec-grep-artifacts-") && name.endsWith(".complete"),
    ),
  );
});

test("plans only missing artifacts and replaces the plan when falling back", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-plans-");
  const tokenizerBytes = Buffer.from("{}");
  const tokenizer = {
    path: "tokenizer.json",
    size: tokenizerBytes.length,
    sha256: createHash("sha256").update(tokenizerBytes).digest("hex"),
  };
  await mkdir(join(root, "huggingface", "onnx"), { recursive: true });
  await writeFile(join(root, "huggingface", artifact.path), bytes);
  await mkdir(join(root, "modelscope"));
  await writeFile(join(root, "modelscope", tokenizer.path), tokenizerBytes);
  const plans = [];
  const requested = [];
  const result = await resolveModelArtifacts(
    options(root, {
      artifacts: [artifact, tokenizer],
      onDownloadPlan(artifacts) {
        plans.push(artifacts);
      },
      dependencies: {
        async fetch(url) {
          requested.push(url);
          if (url.startsWith("https://huggingface.co/")) {
            assert.deepEqual(plans, [[tokenizer]]);
            return new Response(null, { status: 503 });
          }
          assert.deepEqual(plans, [[tokenizer], [artifact]]);
          return new Response(bytes);
        },
      },
    }),
  );
  assert.equal(result.source.kind, "modelscope");
  assert.equal(requested.length, 2);
  assert.ok(requested[0].endsWith("/tokenizer.json"));
  assert.ok(requested[1].endsWith("/onnx/model%20q4.onnx"));
  assert.deepEqual(
    await readFile(result.paths[tokenizer.path]),
    tokenizerBytes,
  );
});

test("concurrent completion-marker writers leave no temporary files", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-marker-race-");
  const directory = join(root, "huggingface");
  await mkdir(join(directory, "onnx"), { recursive: true });
  await writeFile(join(directory, artifact.path), bytes);

  await Promise.all(
    Array.from({ length: 20 }, () =>
      resolveModelArtifacts(
        options(root, {
          dependencies: {
            async fetch() {
              throw new Error("a valid cache must not access the network");
            },
          },
        }),
      ),
    ),
  );

  assert.deepEqual(
    (await readdir(directory)).filter(
      (name) => name.includes(".part-") || name.includes(".replaced-"),
    ),
    [],
  );
});

test("rejects a same-size corrupt cache and atomically installs the verified HF artifact", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-download-");
  const huggingFaceDirectory = join(root, "huggingface");
  const destination = join(huggingFaceDirectory, artifact.path);
  await mkdir(join(huggingFaceDirectory, "onnx"), { recursive: true });
  await writeFile(destination, Buffer.alloc(bytes.byteLength, 120));
  const calls = [];
  const progress = [];

  const result = await resolveModelArtifacts(
    options(root, {
      onProgress: (value) => progress.push(value),
      dependencies: {
        async fetch(url, request) {
          assert.equal(progress.at(-1)?.downloadedBytes, 0);
          calls.push({ url, request });
          return new Response(bytes);
        },
      },
    }),
  );

  assert.equal(result.source.kind, "huggingface");
  assert.deepEqual(await readFile(destination), bytes);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].request.redirect, "follow");
  assert.ok(calls[0].request.signal instanceof AbortSignal);
  assert.equal(progress.at(-1).downloadedBytes, bytes.byteLength);
  assert.equal(progress.at(-1).totalBytes, bytes.byteLength);
  assert.equal(progress.at(-1).source, "huggingface");
  assert.deepEqual(
    (await readdir(join(huggingFaceDirectory, "onnx"))).filter(
      (name) => name.includes(".part-") || name.includes(".replaced-"),
    ),
    [],
  );
});

test("does not trust a completion marker after a same-size file mutation", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-marker-");
  let fetchCalls = 0;
  const resolveOptions = options(root, {
    dependencies: {
      async fetch() {
        fetchCalls++;
        return new Response(bytes);
      },
    },
  });
  const first = await resolveModelArtifacts(resolveOptions);
  assert.equal(fetchCalls, 1);

  await new Promise((resolve) => setTimeout(resolve, 2));
  await writeFile(
    first.paths[artifact.path],
    Buffer.alloc(bytes.byteLength, 120),
  );
  await resolveModelArtifacts(resolveOptions);

  assert.equal(fetchCalls, 2);
  assert.deepEqual(await readFile(first.paths[artifact.path]), bytes);
  assert.deepEqual(
    (await readdir(join(root, "huggingface"))).filter((name) =>
      name.includes(".replaced-"),
    ),
    [],
  );
});

test("never removes an artifact destination changed by another writer", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-race-");
  const destination = join(root, "huggingface", artifact.path);
  await mkdir(join(root, "huggingface", "onnx"), { recursive: true });
  await writeFile(destination, Buffer.alloc(bytes.byteLength, 120));
  let releaseBody;
  const bodyGate = new Promise((resolve) => {
    releaseBody = resolve;
  });
  t.after(() => releaseBody());
  let fetchCalls = 0;
  let bodySent = false;
  const resolution = resolveModelArtifacts(
    options(root, {
      dependencies: {
        async fetch() {
          fetchCalls++;
          return new Response(
            new ReadableStream({
              async pull(controller) {
                if (bodySent) {
                  return;
                }
                bodySent = true;
                await bodyGate;
                controller.enqueue(bytes);
                controller.close();
              },
            }),
          );
        },
      },
    }),
  );
  while (fetchCalls === 0) {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }

  const concurrentBytes = Buffer.alloc(bytes.byteLength, 121);
  await new Promise((resolve) => setTimeout(resolve, 2));
  await writeFile(destination, concurrentBytes);
  releaseBody();

  await assert.rejects(resolution, (error) => {
    assert.ok(error instanceof ArtifactDownloadError);
    assert.equal(error.kind, "filesystem");
    return true;
  });
  assert.equal(fetchCalls, 1, "a local race must not trigger source fallback");
  assert.deepEqual(await readFile(destination), concurrentBytes);
});

for (const status of [403, 404, 408, 429, 500, 503]) {
  test(`falls back once from HTTP ${status} and uses the ModelScope URL`, async (t) => {
    const root = await createTemporaryDirectory(t, `zvec-artifact-${status}-`);
    const calls = [];
    const warnings = [];
    const result = await resolveModelArtifacts(
      options(root, {
        onFallback: (warning) => warnings.push(warning),
        dependencies: {
          async fetch(url) {
            calls.push(url);
            return calls.length === 1
              ? new Response(null, { status, statusText: "Unavailable" })
              : new Response(bytes);
          },
        },
      }),
    );

    assert.equal(result.source.kind, "modelscope");
    assert.equal(warnings.length, 1);
    assert.match(warnings[0], /falling back to ModelScope/u);
    assert.equal(
      calls[0],
      "https://huggingface.co/owner/model/resolve/hf-revision/onnx/model%20q4.onnx",
    );
    assert.equal(
      calls[1],
      "https://modelscope.cn/models/iic/model/resolve/ms-revision/onnx/model%20q4.onnx",
    );
  });
}

test("does not fall back for HTTP 401", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-401-");
  let fetchCalls = 0;
  let warnings = 0;
  await assert.rejects(
    resolveModelArtifacts(
      options(root, {
        onFallback() {
          warnings++;
        },
        dependencies: {
          async fetch() {
            fetchCalls++;
            return new Response(null, { status: 401 });
          },
        },
      }),
    ),
    (error) => {
      assert.ok(error instanceof ArtifactDownloadError);
      assert.equal(error.kind, "http");
      assert.equal(error.status, 401);
      assert.equal(isFallbackEligibleArtifactError(error), false);
      return true;
    },
  );
  assert.equal(fetchCalls, 1);
  assert.equal(warnings, 0);
});

test("treats a fetch AbortError as non-fallback caller cancellation", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-abort-");
  let fetchCalls = 0;
  await assert.rejects(
    resolveModelArtifacts(
      options(root, {
        dependencies: {
          async fetch() {
            fetchCalls++;
            throw new DOMException("cancelled", "AbortError");
          },
        },
      }),
    ),
    (error) => {
      assert.ok(error instanceof ArtifactDownloadError);
      assert.equal(error.kind, "aborted");
      assert.equal(error.fallbackAllowed, false);
      return true;
    },
  );
  assert.equal(fetchCalls, 1);
});

test("aborts a request at the response-header deadline and falls back", async (t) => {
  const root = await createTemporaryDirectory(
    t,
    "zvec-artifact-header-timeout-",
  );
  let requestWasAborted = false;
  let fetchCalls = 0;
  const result = await resolveModelArtifacts(
    options(root, {
      timeouts: { responseHeaderMs: 5, readIdleMs: 50 },
      dependencies: {
        async fetch(_url, request) {
          fetchCalls++;
          if (fetchCalls > 1) {
            return new Response(bytes);
          }
          return await new Promise((_resolve, reject) => {
            request.signal.addEventListener(
              "abort",
              () => {
                requestWasAborted = true;
                reject(new DOMException("aborted by fetch", "AbortError"));
              },
              { once: true },
            );
          });
        },
      },
    }),
  );

  assert.equal(result.source.kind, "modelscope");
  assert.equal(requestWasAborted, true);
});

test("resets the read-idle deadline for each chunk and has no total deadline", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-idle-reset-");
  const chunks = [...bytes].map((value) => Uint8Array.of(value));
  const responseBody = new ReadableStream({
    async pull(controller) {
      await new Promise((resolve) => setTimeout(resolve, 2));
      const chunk = chunks.shift();
      if (chunk) {
        controller.enqueue(chunk);
      } else {
        controller.close();
      }
    },
  });

  const result = await resolveModelArtifacts(
    options(root, {
      timeouts: { responseHeaderMs: 10, readIdleMs: 20 },
      dependencies: {
        async fetch() {
          return new Response(responseBody);
        },
      },
    }),
  );

  assert.equal(result.source.kind, "huggingface");
  assert.deepEqual(await readFile(result.paths[artifact.path]), bytes);
});

test("aborts a stalled body read and falls back", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-idle-timeout-");
  let fetchCalls = 0;
  let requestWasAborted = false;
  const result = await resolveModelArtifacts(
    options(root, {
      timeouts: { responseHeaderMs: 50, readIdleMs: 5 },
      dependencies: {
        async fetch(_url, request) {
          fetchCalls++;
          if (fetchCalls > 1) {
            return new Response(bytes);
          }
          const stream = new ReadableStream({
            start(controller) {
              request.signal.addEventListener(
                "abort",
                () => {
                  requestWasAborted = true;
                  controller.error(
                    new DOMException("aborted by fetch", "AbortError"),
                  );
                },
                { once: true },
              );
            },
          });
          return new Response(stream);
        },
      },
    }),
  );

  assert.equal(result.source.kind, "modelscope");
  assert.equal(requestWasAborted, true);
});

test("falls back after network/TLS failure", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-network-");
  let fetchCalls = 0;
  const result = await resolveModelArtifacts(
    options(root, {
      dependencies: {
        async fetch() {
          fetchCalls++;
          if (fetchCalls === 1) {
            throw new TypeError("fetch failed", {
              cause: Object.assign(new Error("TLS handshake failed"), {
                code: "CERT_HAS_EXPIRED",
              }),
            });
          }
          return new Response(bytes);
        },
      },
    }),
  );
  assert.equal(result.source.kind, "modelscope");
});

test("falls back after an interrupted stream or integrity mismatch", async (t) => {
  for (const failure of ["interrupted", "integrity"]) {
    await t.test(failure, async () => {
      const root = await createTemporaryDirectory(
        t,
        `zvec-artifact-${failure}-`,
      );
      let fetchCalls = 0;
      const result = await resolveModelArtifacts(
        options(root, {
          dependencies: {
            async fetch() {
              fetchCalls++;
              if (fetchCalls > 1) {
                return new Response(bytes);
              }
              if (failure === "integrity") {
                return new Response(Buffer.alloc(bytes.byteLength, 42));
              }
              return new Response(
                new ReadableStream({
                  pull(controller) {
                    controller.error(new Error("socket reset"));
                  },
                }),
              );
            },
          },
        }),
      );
      assert.equal(result.source.kind, "modelscope");
      assert.equal(fetchCalls, 2);
    });
  }
});

test("preserves both source failures in a resolution error", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-both-fail-");
  let fetchCalls = 0;
  await assert.rejects(
    resolveModelArtifacts(
      options(root, {
        dependencies: {
          async fetch() {
            fetchCalls++;
            return fetchCalls === 1
              ? new Response(null, { status: 503 })
              : new Response(Buffer.from("wrong"));
          },
        },
      }),
    ),
    (error) => {
      assert.ok(error instanceof ModelArtifactResolutionError);
      assert.equal(error.errors.length, 2);
      assert.equal(error.primaryError.kind, "http");
      assert.equal(error.primaryError.status, 503);
      assert.equal(error.fallbackError.kind, "integrity");
      assert.match(error.message, /HTTP 503/);
      assert.match(error.message, /Integrity check failed/);
      assert.match(error.message, /huggingface/);
      assert.match(error.message, /modelscope/);
      return true;
    },
  );
});

test("a local filesystem failure never triggers source fallback", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-fs-");
  const cacheFile = join(root, "not-a-directory");
  await writeFile(cacheFile, "file");
  let fetchCalls = 0;
  await assert.rejects(
    resolveModelArtifacts(
      options(root, {
        sources: sources(root, {
          huggingface: { cacheDirectory: cacheFile },
        }),
        dependencies: {
          async fetch() {
            fetchCalls++;
            return new Response(bytes);
          },
        },
      }),
    ),
    (error) => {
      assert.ok(error instanceof ArtifactDownloadError);
      assert.equal(error.kind, "filesystem");
      assert.equal(error.fallbackAllowed, false);
      return true;
    },
  );
  assert.equal(fetchCalls, 0);
});

test("serializes concurrent callers for the same snapshot and rechecks cache", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-lock-");
  let releaseFetch;
  const fetchGate = new Promise((resolve) => {
    releaseFetch = resolve;
  });
  let fetchCalls = 0;
  const resolveOptions = options(root, {
    dependencies: {
      async fetch() {
        fetchCalls++;
        await fetchGate;
        return new Response(bytes);
      },
    },
  });

  const first = resolveModelArtifacts(resolveOptions);
  while (fetchCalls === 0) {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  const second = resolveModelArtifacts(resolveOptions);
  await new Promise((resolve) => setTimeout(resolve, 10));
  releaseFetch();

  const [firstResult, secondResult] = await Promise.all([first, second]);
  assert.equal(fetchCalls, 1);
  assert.equal(firstResult.source.kind, "huggingface");
  assert.equal(secondResult.source.kind, "huggingface");
  assert.equal(
    firstResult.paths[artifact.path],
    secondResult.paths[artifact.path],
  );
});

test("recovers a stale snapshot lock and prevents the stale owner from continuing", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-stale-lock-");
  let releaseFirstFetch;
  let releaseSecondFetch;
  const firstFetchGate = new Promise((resolve) => {
    releaseFirstFetch = resolve;
  });
  const secondFetchGate = new Promise((resolve) => {
    releaseSecondFetch = resolve;
  });
  t.after(() => {
    releaseFirstFetch();
    releaseSecondFetch();
  });
  let fetchCalls = 0;
  let clock = Date.now();
  const resolveOptions = options(root, {
    lock: { pollMs: 2, staleMs: 10, heartbeatMs: 1 },
    dependencies: {
      now() {
        return clock;
      },
      async fetch() {
        fetchCalls++;
        if (fetchCalls === 1) {
          await firstFetchGate;
        } else if (fetchCalls === 2) {
          await secondFetchGate;
        }
        return new Response(bytes);
      },
    },
  });

  const staleOwner = resolveModelArtifacts(resolveOptions);
  while (fetchCalls < 1) {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  const cacheDirectory = join(root, "huggingface");
  while (true) {
    const names = await readdir(cacheDirectory).catch(() => []);
    if (names.some((candidate) => candidate.endsWith(".lock"))) {
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  clock += 100;

  const recovered = resolveModelArtifacts(resolveOptions);
  while (fetchCalls < 2) {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  assert.equal(fetchCalls, 2);

  await new Promise((resolve) => setTimeout(resolve, 2));
  releaseFirstFetch();
  await assert.rejects(staleOwner, (error) => {
    assert.ok(error instanceof ArtifactDownloadError);
    assert.equal(error.kind, "filesystem");
    return true;
  });
  assert.ok(
    (await readdir(cacheDirectory)).some((name) => name.endsWith(".lock")),
    "the stale owner must not unlink the replacement owner's lock",
  );

  releaseSecondFetch();
  assert.equal((await recovered).source.kind, "huggingface");
});

test("uses injected deadline timers and clears them after each successful phase", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-timers-");
  const scheduled = [];
  const cleared = [];
  let nextHandle = 0;
  await resolveModelArtifacts(
    options(root, {
      dependencies: {
        async fetch() {
          return new Response(bytes);
        },
        setTimeout(callback, milliseconds) {
          const handle = { id: ++nextHandle, callback, milliseconds };
          scheduled.push(handle);
          return handle;
        },
        clearTimeout(handle) {
          cleared.push(handle);
        },
      },
    }),
  );

  assert.deepEqual(
    scheduled.map(({ milliseconds }) => milliseconds),
    [10_000, 30_000, 30_000],
  );
  assert.deepEqual(cleared, scheduled);
});

test("callback failures are local and never trigger source fallback", async (t) => {
  for (const callback of ["plan", "progress", "fallback"]) {
    await t.test(callback, async () => {
      const root = await createTemporaryDirectory(
        t,
        `zvec-artifact-${callback}-callback-`,
      );
      let fetchCalls = 0;
      await assert.rejects(
        resolveModelArtifacts(
          options(root, {
            [{
              plan: "onDownloadPlan",
              progress: "onProgress",
              fallback: "onFallback",
            }[callback]]() {
              throw new Error(`${callback} consumer failed`);
            },
            dependencies: {
              async fetch() {
                fetchCalls++;
                return callback === "fallback"
                  ? new Response(null, { status: 503 })
                  : new Response(bytes);
              },
            },
          }),
        ),
        (error) => {
          assert.ok(error instanceof ArtifactDownloadError);
          assert.equal(error.kind, "callback");
          assert.equal(error.fallbackAllowed, false);
          return true;
        },
      );
      assert.equal(fetchCalls, callback === "fallback" ? 1 : 0);
    });
  }
});

test("validates resolver recipes before filesystem or network access", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-input-");
  const base = options(root);
  const secondArtifact = {
    ...artifact,
    path: "tokenizer.json",
  };
  const cases = [
    ["empty model", { ...base, model: " " }],
    ["empty artifacts", { ...base, artifacts: [] }],
    ["empty sources", { ...base, sources: [] }],
    [
      "duplicate source",
      { ...base, sources: [base.sources[0], { ...base.sources[0] }] },
    ],
    ["duplicate artifact", { ...base, artifacts: [artifact, { ...artifact }] }],
    ["negative size", { ...base, artifacts: [{ ...artifact, size: -1 }] }],
    ["invalid hash", { ...base, artifacts: [{ ...artifact, sha256: "bad" }] }],
    [
      "unknown mapping",
      {
        ...base,
        sources: [
          base.sources[0],
          {
            ...base.sources[1],
            localPaths: { "unknown.bin": "unknown.bin" },
          },
        ],
      },
    ],
    [
      "unknown source",
      {
        ...base,
        sources: [{ ...base.sources[0], kind: "mirror" }],
      },
    ],
    [
      "unsafe repo",
      {
        ...base,
        sources: [{ ...base.sources[0], repo: "../model" }],
      },
    ],
    [
      "unsafe revision",
      {
        ...base,
        sources: [{ ...base.sources[0], revision: ".." }],
      },
    ],
    [
      "invalid cache",
      {
        ...base,
        sources: [{ ...base.sources[0], cacheDirectory: "bad\0cache" }],
      },
    ],
    [
      "duplicate local destination",
      {
        ...base,
        artifacts: [artifact, secondArtifact],
        sources: [
          {
            ...base.sources[0],
            localPaths: {
              [artifact.path]: "same.bin",
              [secondArtifact.path]: "same.bin",
            },
          },
        ],
      },
    ],
    [
      "zero timeout",
      { ...base, timeouts: { responseHeaderMs: 0, readIdleMs: 1 } },
    ],
    [
      "invalid lock heartbeat",
      { ...base, lock: { pollMs: 1, staleMs: 2, heartbeatMs: 2 } },
    ],
  ];

  for (const [name, candidate] of cases) {
    await t.test(name, async () => {
      await assert.rejects(resolveModelArtifacts(candidate), (error) => {
        assert.ok(error instanceof ArtifactDownloadError);
        assert.equal(error.kind, "invalid-input");
        return true;
      });
    });
  }
});

test("rejects unsafe local mappings before touching the network", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-path-");
  let fetchCalls = 0;
  await assert.rejects(
    resolveModelArtifacts(
      options(root, {
        sources: sources(root, {
          modelscope: {
            localPaths: { [artifact.path]: "../escape.onnx" },
          },
        }),
        dependencies: {
          async fetch() {
            fetchCalls++;
            return new Response(bytes);
          },
        },
      }),
    ),
    (error) => {
      assert.ok(error instanceof ArtifactDownloadError);
      assert.equal(error.kind, "invalid-input");
      return true;
    },
  );
  assert.equal(fetchCalls, 0);
});
