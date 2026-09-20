import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { listEmbeddingModels } from "../../../dist/engine/models/catalog.js";

const EXPECTED_LOCAL_METADATA = {
  "local/embeddinggemma-300m": {
    huggingFace: [
      "ggml-org/embeddinggemma-300M-GGUF",
      "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12",
    ],
    modelScope: [
      "ggml-org/embeddinggemma-300M-GGUF",
      "e2fab2963ac0943b1dc320cf0e267bd0e06e2e97",
    ],
    artifacts: ["embeddinggemma-300M-Q8_0.gguf"],
    artifactBytes: 333590944,
    artifactManifestSha256:
      "837b9fcffa1628422e6f505dc6cffab06af281ce6369f825847b50b76234d8b3",
  },
  "local/qwen3-embedding-0.6b": {
    huggingFace: [
      "Qwen/Qwen3-Embedding-0.6B-GGUF",
      "370f27d7550e0def9b39c1f16d3fbaa13aa67728",
    ],
    modelScope: [
      "Qwen/Qwen3-Embedding-0.6B-GGUF",
      "61ae123505fc6fa7f36d372fae1a30bc384241ea",
    ],
    artifacts: ["Qwen3-Embedding-0.6B-Q8_0.gguf"],
    artifactBytes: 639150592,
    artifactManifestSha256:
      "68adbe97d0daf27fbf14be0935252ff60e8b9b1f9650bffb4bc9fd6b0660940e",
  },
  "local/bge-small-en-v1.5": {
    huggingFace: [
      "onnx-community/bge-small-en-v1.5-ONNX",
      "4a9a46c7b88fa408e650a571a1800243f26309bd",
    ],
    modelScope: [
      "onnx-community/bge-small-en-v1.5-ONNX",
      "f246b360b061b613fe0b449f2e14de12f875dad7",
    ],
    artifacts: [
      "config.json",
      "tokenizer.json",
      "tokenizer_config.json",
      "onnx/model_q4.onnx",
      "onnx/model_q4.onnx_data",
    ],
    artifactBytes: 61853327,
    artifactManifestSha256:
      "e799cb353abd1f7da71c41358304c0317b5b76545bcfa4d11974c523fa08f77f",
  },
  "local/all-minilm-l6-v2": {
    huggingFace: [
      "onnx-community/all-MiniLM-L6-v2-ONNX",
      "aff7a1dc4e8a1ea593e6ea21e95c22ef0a25966f",
    ],
    modelScope: [
      "onnx-community/all-MiniLM-L6-v2-ONNX",
      "e1da369847063d70f2fd772226551865bcab1c2d",
    ],
    artifacts: [
      "config.json",
      "tokenizer.json",
      "tokenizer_config.json",
      "onnx/model_q4.onnx",
      "onnx/model_q4.onnx_data",
    ],
    artifactBytes: 55035424,
    artifactManifestSha256:
      "6ffb1a32b434f8d07a6f4dbc68d55ef664c8cf2bc1b3c898639713550e0ade1c",
  },
  "local/potion-retrieval-32m": {
    huggingFace: [
      "minishlab/potion-retrieval-32M",
      "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
    ],
    modelScope: [
      "minishlab/potion-retrieval-32M",
      "33da23fc75cb732b5370bf25adde3db74b0d65b3",
    ],
    artifacts: ["model.safetensors", "tokenizer.json"],
    artifactBytes: 130703606,
    artifactManifestSha256:
      "b01b072057dcbfb17e676fbd26f489a84b235093e98ea684fbcbcafe5535040c",
  },
  "local/potion-multilingual-128m": {
    huggingFace: [
      "minishlab/potion-multilingual-128M",
      "73908c3438cf03b6a01bcb9611d62b23d0726f08",
    ],
    modelScope: [
      "minishlab/potion-multilingual-128M",
      "e8524678123f281add99f9745ac33d6604434dd7",
    ],
    artifacts: ["model.safetensors", "tokenizer.json"],
    artifactBytes: 530977691,
    artifactManifestSha256:
      "48cb19acfb21d4d77b0f92f83394dcf2260c3eeb72e4591a8175e9c139fdf42c",
  },
  "local/potion-code-16m-v2": {
    huggingFace: [
      "minishlab/potion-code-16M-v2",
      "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
    ],
    modelScope: [
      "minishlab/potion-code-16M-v2",
      "3e922fde18f43b8db69f3381b6d468738d3dd2d7",
    ],
    artifacts: ["model.safetensors", "tokenizer.json"],
    artifactBytes: 33514412,
    artifactManifestSha256:
      "857174683f92a0ee2462ccde3609f768693254b8e31ce1d40899b4fb32c0125c",
  },
  "local/multilingual-e5-small": {
    huggingFace: [
      "Xenova/multilingual-e5-small",
      "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
    ],
    modelScope: [
      "Xenova/multilingual-e5-small",
      "252d0dcb679dda2c7b6fd5bbfed15df3c7feaebf",
    ],
    artifacts: [
      "config.json",
      "tokenizer.json",
      "tokenizer_config.json",
      "onnx/model_quantized.onnx",
    ],
    artifactBytes: 135392016,
    artifactManifestSha256:
      "81d1e8f84edb9046738963f2ea0dba31bb123c5aaf681adf2744f6ce888b1a56",
  },
  "local/jina-embeddings-v2-base-code": {
    huggingFace: [
      "jinaai/jina-embeddings-v2-base-code",
      "516f4baf13dec4ddddda8631e019b5737c8bc250",
    ],
    modelScope: [
      "jinaai/jina-embeddings-v2-base-code",
      "91aa0a6aa801c408149324e32e8cd43f8502da8f",
    ],
    artifacts: [
      "config.json",
      "tokenizer.json",
      "tokenizer_config.json",
      "onnx/model_quantized.onnx",
    ],
    artifactBytes: 164458646,
    artifactManifestSha256:
      "fb32b56c9e7757072846ea185e839c5df73bc785a5f81001a53163aff6445bbe",
  },
  "local/gte-modernbert-base": {
    huggingFace: [
      "Alibaba-NLP/gte-modernbert-base",
      "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
    ],
    modelScope: [
      "iic/gte-modernbert-base",
      "678f4ed93760af288132f4f9dc5b6daebdc48777",
    ],
    artifacts: [
      "config.json",
      "tokenizer.json",
      "tokenizer_config.json",
      "onnx/model_q4.onnx",
    ],
    artifactBytes: 227758040,
    artifactManifestSha256:
      "f5dc6505d360924869fe39e6e28bee1cb1f70cdb3f0f35387957b20d02dc79ca",
  },
  "local/nomic-embed-text-v1.5": {
    huggingFace: [
      "nomic-ai/nomic-embed-text-v1.5",
      "e9b6763023c676ca8431644204f50c2b100d9aab",
    ],
    modelScope: [
      "nomic-ai/nomic-embed-text-v1.5",
      "c6fb77fdf73531ee8319b34e46f7e749b59e74e8",
    ],
    artifacts: [
      "config.json",
      "tokenizer.json",
      "tokenizer_config.json",
      "onnx/model_q4.onnx",
    ],
    artifactBytes: 165828346,
    artifactManifestSha256:
      "678704daf5caf3f5c2b511ffce8ac657ca98cb10273de198ae5d4a4e5c03ea8a",
  },
};

test("local embedding models declare pinned source and artifact metadata", () => {
  const localEntries = listEmbeddingModels().filter(
    (entry) => entry.provider === "local",
  );

  assert.equal(localEntries.length, 11);
  assert.deepEqual(
    localEntries.map((entry) => entry.reference).sort(),
    Object.keys(EXPECTED_LOCAL_METADATA).sort(),
  );

  for (const entry of localEntries) {
    const expected = EXPECTED_LOCAL_METADATA[entry.reference];
    assert.ok(expected, `missing expectation for ${entry.reference}`);

    for (const sourceName of ["huggingFace", "modelScope"]) {
      const source = entry.sources[sourceName];
      assert.deepEqual(
        [source.repo, source.revision],
        expected[sourceName],
        `${entry.reference} ${sourceName}`,
      );
      assert.match(source.revision, /^[0-9a-f]{40}$/);
    }

    assert.deepEqual(
      entry.artifacts.map((artifact) => artifact.path),
      expected.artifacts,
      `${entry.reference} artifacts`,
    );
    assert.equal(
      entry.artifacts.reduce((total, artifact) => total + artifact.size, 0),
      expected.artifactBytes,
      `${entry.reference} artifact bytes`,
    );
    assert.equal(
      createHash("sha256")
        .update(JSON.stringify(entry.artifacts))
        .digest("hex"),
      expected.artifactManifestSha256,
      `${entry.reference} artifact manifest`,
    );
    for (const artifact of entry.artifacts) {
      assert.ok(artifact.size > 0, `${entry.reference} ${artifact.path} size`);
      assert.match(
        artifact.sha256,
        /^[0-9a-f]{64}$/,
        `${entry.reference} ${artifact.path} sha256`,
      );
    }
  }
});

test("GGUF URIs pin Hugging Face revisions without changing cache names", () => {
  const ggufEntries = listEmbeddingModels().filter(
    (entry) => entry.backend === "llama-cpp",
  );

  assert.equal(ggufEntries.length, 2);
  for (const entry of ggufEntries) {
    assert.ok(
      entry.uri.endsWith(`#${entry.sources.huggingFace.revision}`),
      `${entry.reference} URI must pin its Hugging Face revision`,
    );
    assert.match(entry.cacheFile, /^hf_.+\.gguf$/);
  }
});

test("GTE uses the ModelScope IIC namespace", () => {
  const gte = listEmbeddingModels().find(
    (entry) => entry.reference === "local/gte-modernbert-base",
  );

  assert.equal(gte.sources.huggingFace.repo, "Alibaba-NLP/gte-modernbert-base");
  assert.equal(gte.sources.modelScope.repo, "iic/gte-modernbert-base");
});
