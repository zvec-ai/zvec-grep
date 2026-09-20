import { Tokenizer } from "@huggingface/tokenizers";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { TokenizerLike } from "./model2vec-runtime.js";

type TokenizerJson = {
  model?: {
    unk_id?: unknown;
    unk_token?: unknown;
  };
};

export async function loadModel2VecTokenizer(
  source: string,
): Promise<TokenizerLike> {
  const [tokenizerJson, tokenizerConfig] = await Promise.all([
    readJson(join(source, "tokenizer.json")),
    readJson(join(source, "tokenizer_config.json")),
  ]);
  const tokenizer = new Tokenizer(tokenizerJson, tokenizerConfig);
  const unknownTokenId = resolveUnknownTokenId(tokenizer, tokenizerJson);
  const tokenize = (
    text: string,
    options: {
      add_special_tokens: false;
      truncation: true;
      max_length: number;
    },
  ) => {
    const ids = tokenizer.encode(text, {
      add_special_tokens: options.add_special_tokens,
    }).ids;
    return {
      input_ids: {
        data:
          ids.length > options.max_length
            ? ids.slice(0, options.max_length)
            : ids,
      },
    };
  };

  return Object.assign(tokenize, { unk_token_id: unknownTokenId });
}

async function readJson(path: string): Promise<Record<string, unknown>> {
  return JSON.parse(await readFile(path, "utf8")) as Record<string, unknown>;
}

function resolveUnknownTokenId(
  tokenizer: Tokenizer,
  tokenizerJson: TokenizerJson,
): number | null {
  const configuredId = tokenizerJson.model?.unk_id;
  if (typeof configuredId === "number" && Number.isInteger(configuredId)) {
    return configuredId;
  }

  const configuredToken = tokenizerJson.model?.unk_token;
  if (typeof configuredToken !== "string") {
    return null;
  }
  const tokenId: unknown = tokenizer.token_to_id(configuredToken);
  return typeof tokenId === "number" && Number.isInteger(tokenId)
    ? tokenId
    : null;
}
