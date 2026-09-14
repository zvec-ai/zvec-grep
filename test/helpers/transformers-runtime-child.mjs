import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { TransformersJsEmbeddingModel } from "../../dist/engine/models/backends/transformers-js.js";

// Encode the small subset of protobuf needed for this synthetic ONNX graph.
// The graph casts token IDs to floats, then adds the hidden-size dimension:
// input_ids [batch, sequence] -> last_hidden_state [batch, sequence, 1].
// This exercises the real tokenizer, ONNX session, pooling, and adapter output.
function varint(value) {
  const bytes = [];
  do {
    const byte = value & 0x7f;
    value >>>= 7;
    bytes.push(value ? byte | 0x80 : byte);
  } while (value);
  return Buffer.from(bytes);
}

function integer(field, value) {
  return Buffer.concat([varint(field * 8), varint(value)]);
}

function message(field, value) {
  const bytes =
    typeof value === "string" ? Buffer.from(value) : Buffer.concat(value);
  return Buffer.concat([varint(field * 8 + 2), varint(bytes.length), bytes]);
}

function tensorInfo(field, name, type, dimensions) {
  return message(field, [
    message(1, name),
    message(2, [
      message(1, [
        integer(1, type),
        message(
          2,
          dimensions.map((dimension) =>
            message(1, [
              typeof dimension === "string"
                ? message(2, dimension)
                : integer(1, dimension),
            ]),
          ),
        ),
      ]),
    ]),
  ]);
}

function tinyOnnxModel() {
  return Buffer.concat([
    integer(1, 8), // ModelProto.ir_version
    message(7, [
      message(1, [
        message(1, "input_ids"),
        message(2, "floats"),
        message(4, "Cast"),
        message(5, [message(1, "to"), integer(3, 1), integer(20, 2)]),
      ]),
      message(1, [
        message(1, "floats"),
        message(2, "last_hidden_state"),
        message(4, "Unsqueeze"),
        message(5, [message(1, "axes"), integer(8, 2), integer(20, 7)]),
      ]),
      message(2, "synthetic-feature-extraction"),
      tensorInfo(11, "input_ids", 7, ["batch", "sequence"]),
      tensorInfo(12, "last_hidden_state", 1, ["batch", "sequence", 1]),
    ]),
    message(8, [integer(2, 11)]), // OperatorSetIdProto.version
  ]);
}

async function createFixture(
  modelCacheDir,
  {
    invalidOnnx = false,
    invalidTokenizer = false,
    model = "tiny-transformer",
  } = {},
) {
  const source = { repo: `test/${model}`, revision: "local-fixture" };
  const directory = join(modelCacheDir, source.repo, source.revision);
  const files = {
    "onnx/model.onnx": invalidOnnx
      ? Buffer.from("not a valid ONNX model")
      : tinyOnnxModel(),
    "config.json": {
      model_type: "bert",
      hidden_size: 1,
      num_hidden_layers: 1,
      num_attention_heads: 1,
      vocab_size: 6,
    },
    "tokenizer_config.json": {
      tokenizer_class: "BertTokenizer",
      unk_token: "[UNK]",
      sep_token: "[SEP]",
      pad_token: "[PAD]",
      cls_token: "[CLS]",
      model_max_length: 32,
    },
    "tokenizer.json": {
      version: "1.0",
      added_tokens: [],
      normalizer: {
        type: "BertNormalizer",
        clean_text: true,
        handle_chinese_chars: true,
        strip_accents: null,
        lowercase: true,
      },
      pre_tokenizer: { type: "BertPreTokenizer" },
      post_processor: null,
      decoder: { type: "WordPiece", prefix: "##", cleanup: true },
      model: {
        type: "WordPiece",
        unk_token: "[UNK]",
        continuing_subword_prefix: "##",
        max_input_chars_per_word: 100,
        vocab: {
          "[UNK]": 0,
          "[PAD]": 1,
          "[CLS]": 2,
          "[SEP]": 3,
          hello: 4,
          world: 5,
        },
      },
    },
  };
  if (invalidTokenizer) {
    files["tokenizer.json"] = {};
  }
  const artifacts = [];
  for (const [path, value] of Object.entries(files)) {
    const bytes = Buffer.isBuffer(value)
      ? value
      : Buffer.from(JSON.stringify(value));
    const filename = join(directory, path);
    await mkdir(dirname(filename), { recursive: true });
    await writeFile(filename, bytes);
    artifacts.push({
      path,
      size: bytes.length,
      sha256: createHash("sha256").update(bytes).digest("hex"),
    });
  }
  return {
    backend: "transformers-js",
    reference: `local/${model}`,
    provider: "local",
    model,
    repo: source.repo,
    revision: source.revision,
    sources: { huggingFace: source, modelScope: source },
    artifacts,
    dtype: "fp32",
    dimension: 1,
    metric: "cosine",
    pooling: "mean",
    normalize: false,
    maxInputTokens: 32,
    maxBatchSize: 32,
  };
}

// An initialization failure can poison the installed Transformers.js runtime.
// Each scenario therefore runs in its own process, without replacing the
// adapter dependencies or patching any Transformers.js internals.
const scenario = process.argv[2];
const modelCacheDir = await mkdtemp(join(tmpdir(), "zg-transformers-runtime-"));
const models = [];
const progress = [];
let networkRequests = 0;
globalThis.fetch = async () => {
  networkRequests++;
  throw new Error("The Transformers.js runtime regression must stay offline");
};

try {
  const entry = await createFixture(modelCacheDir, {
    invalidOnnx: scenario === "failure",
    invalidTokenizer: scenario === "tokenizer-failure",
  });
  const model = new TransformersJsEmbeddingModel(entry, {
    modelCacheDir,
    ...(scenario === "default"
      ? {}
      : {
          device: ["failure", "tokenizer-failure"].includes(scenario)
            ? "cpu"
            : scenario,
        }),
  });
  models.push(model);
  const contents = [
    { kind: "text", text: "hello" },
    { kind: "text", text: "hello world" },
  ];
  const options = { onProgress: (event) => progress.push(event) };

  if (scenario === "failure") {
    let firstFailure;
    const assertLoadFailure = (error) => {
      assert.equal(
        error.code,
        "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_LOAD_FAILED",
      );
      assert.match(error.message + " " + error.context, /restart/i);
      assert.match(error.message + " " + error.context, /cpu/i);
      assert.ok(error.cause instanceof Error);
      firstFailure ??= error;
      assert.equal(error.cause, firstFailure.cause);
      return true;
    };
    await assert.rejects(model.embed(contents, options), assertLoadFailure);
    await Promise.all(
      Array.from({ length: 3 }, () =>
        assert.rejects(model.embed(contents, options), assertLoadFailure),
      ),
    );
    assert.equal(
      progress.filter((event) => event.stage === "preparing").length,
      1,
      "a failed model must not re-enter initialization for later batches",
    );
    assert.equal(
      progress.some((event) => event.stage === "ready"),
      false,
    );
  } else if (scenario === "tokenizer-failure") {
    await assert.rejects(model.embed(contents, options), (error) => {
      assert.equal(
        error.code,
        "ZVEC_GREP.ENGINE.MODELS.TRANSFORMERS_JS_LOAD_FAILED",
      );
      assert.ok(error.cause instanceof Error);
      return true;
    });
    // A tokenizer/config failure is local to its model. It must not prevent
    // another model from using a healthy, shared Transformers.js runtime.
    const validEntry = await createFixture(modelCacheDir, {
      model: "valid-transformer",
    });
    const anotherModel = new TransformersJsEmbeddingModel(validEntry, {
      modelCacheDir,
      device: "cpu",
    });
    models.push(anotherModel);
    assert.deepEqual(await anotherModel.embed(contents), {
      vectors: [[4], [4.5]],
      truncated: [],
    });
  } else {
    assert.deepEqual(await model.embed(contents, options), {
      vectors: [[4], [4.5]],
      truncated: [],
    });
    assert.deepEqual(await model.embed([contents[0]], options), {
      vectors: [[4]],
      truncated: [],
    });
    assert.deepEqual(
      progress.map((event) => event.stage),
      ["preparing", "ready"],
    );
  }
  assert.equal(networkRequests, 0);
} finally {
  await Promise.all(models.map((model) => model.dispose()));
  await rm(modelCacheDir, { recursive: true, force: true });
}
