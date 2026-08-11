import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { loadModel2VecTokenizer } from "../../../dist/engine/models/backends/model2vec-tokenizer.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

test("Model2Vec loads the standalone Hugging Face tokenizer without Transformers", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-tokenizer-");
  await writeFile(
    join(root, "tokenizer.json"),
    JSON.stringify({
      version: "1.0",
      truncation: null,
      padding: null,
      added_tokens: [
        {
          id: 0,
          content: "[UNK]",
          single_word: false,
          lstrip: false,
          rstrip: false,
          normalized: false,
          special: true,
        },
      ],
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
        vocab: { "[UNK]": 0, hello: 1, world: 2, "##s": 3 },
      },
    }),
  );
  await writeFile(
    join(root, "tokenizer_config.json"),
    JSON.stringify({ tokenizer_class: "BertTokenizer" }),
  );

  const tokenizer = await loadModel2VecTokenizer(root);
  const encoded = await tokenizer("Hello worlds mystery", {
    add_special_tokens: false,
    truncation: true,
    max_length: 3,
  });

  assert.deepEqual(encoded.input_ids.data, [1, 2, 3]);
  assert.equal(tokenizer.unk_token_id, 0);
});
