import type { EmbeddingResult } from "../embeddings.js";

export type TokenTensor = {
  data: ArrayLike<number | bigint>;
};

export type TokenizerOutput = {
  input_ids: TokenTensor;
};

export type TokenizerLike = {
  (
    text: string,
    options: {
      add_special_tokens: false;
      truncation: true;
      max_length: number;
    },
  ): TokenizerOutput | Promise<TokenizerOutput>;
  unk_token_id?: number | null;
};

export type StaticEmbeddingTable = {
  data: Float32Array | Uint16Array;
  dimension: number;
  dtype: "F16" | "F32";
  rows: number;
};

export type Model2VecWorkerData = {
  tokenizerSource: string;
  maxInputTokens: number;
  normalize: boolean;
  tableBuffer: SharedArrayBuffer;
  dimension: number;
  dtype: "F16" | "F32";
  rows: number;
};

export type Model2VecWorkerRequest = {
  id: number;
  tokenIds: ArrayBuffer;
  offsets: ArrayBuffer;
};

export type Model2VecTokenizerWorkerRequest = {
  id: number;
  texts: string[];
};

export type SerializedWorkerError = {
  name?: string;
  message: string;
  stack?: string;
};

export type Model2VecWorkerResponse =
  | { type: "ready" }
  | {
      type: "result";
      id: number;
      vectors: ArrayBuffer;
      vectorCount: number;
      truncated: number[];
    }
  | {
      type: "error";
      id: number;
      error: SerializedWorkerError;
    };

export type Model2VecTokenizerWorkerResponse =
  | { type: "ready" }
  | {
      type: "tokenized";
      id: number;
      tokenIds: ArrayBuffer;
      offsets: ArrayBuffer;
      truncated: number[];
    }
  | {
      type: "error";
      id: number;
      error: SerializedWorkerError;
    };

export type TokenizedModel2VecTexts = {
  tokenLists: number[][];
  truncated: number[];
};

export type PackedModel2VecTokenLists = {
  tokenIds: Int32Array;
  offsets: Uint32Array;
};

export async function embedModel2VecTexts(
  texts: readonly string[],
  tokenizer: TokenizerLike,
  table: StaticEmbeddingTable,
  maxInputTokens: number,
  normalize: boolean,
): Promise<EmbeddingResult> {
  const tokenized = await tokenizeModel2VecTexts(
    texts,
    tokenizer,
    maxInputTokens,
  );

  return {
    vectors: embedModel2VecTokenLists(tokenized.tokenLists, table, normalize),
    truncated: tokenized.truncated,
  };
}

export async function tokenizeModel2VecTexts(
  texts: readonly string[],
  tokenizer: TokenizerLike,
  maxInputTokens: number,
): Promise<TokenizedModel2VecTexts> {
  const truncatedInputIndexes: number[] = [];
  const tokenLists = await Promise.all(
    texts.map(async (text, index) => {
      const encoded = await tokenizer(text, {
        add_special_tokens: false,
        truncation: true,
        max_length: maxInputTokens + 1,
      });
      const encodedTokenIds = Array.from(encoded.input_ids.data, Number);
      if (encodedTokenIds.length > maxInputTokens) {
        truncatedInputIndexes.push(index);
      }
      const unknownTokenId = tokenizer.unk_token_id;
      return encodedTokenIds
        .slice(0, maxInputTokens)
        .filter(
          (id) =>
            unknownTokenId === undefined ||
            unknownTokenId === null ||
            id !== unknownTokenId,
        );
    }),
  );

  return {
    tokenLists,
    truncated: truncatedInputIndexes.sort((left, right) => left - right),
  };
}

export function packModel2VecTokenLists(
  tokenLists: readonly (readonly number[])[],
): PackedModel2VecTokenLists {
  const totalTokenCount = tokenLists.reduce(
    (total, tokenIds) => total + tokenIds.length,
    0,
  );
  const tokenIds = new Int32Array(totalTokenCount);
  const offsets = new Uint32Array(tokenLists.length + 1);
  let offset = 0;
  for (const [index, list] of tokenLists.entries()) {
    tokenIds.set(list, offset);
    offset += list.length;
    offsets[index + 1] = offset;
  }
  return { tokenIds, offsets };
}

export function embedPackedModel2VecTokenLists(
  packed: PackedModel2VecTokenLists,
  table: StaticEmbeddingTable,
  normalize: boolean,
): number[][] {
  validatePackedTokenLists(packed);
  const vectors: number[][] = [];
  for (let index = 0; index + 1 < packed.offsets.length; index++) {
    vectors.push(
      embedStaticTokenList(
        packed.tokenIds.subarray(
          packed.offsets[index],
          packed.offsets[index + 1],
        ),
        table,
        normalize,
      ),
    );
  }
  return vectors;
}

export function embedModel2VecTokenLists(
  tokenLists: readonly (readonly number[])[],
  table: StaticEmbeddingTable,
  normalize: boolean,
): number[][] {
  return tokenLists.map((tokenIds) =>
    embedStaticTokenList(tokenIds, table, normalize),
  );
}

export function sharedStaticEmbeddingTable(
  table: StaticEmbeddingTable,
): StaticEmbeddingTable & { data: Float32Array | Uint16Array } {
  if (table.data.buffer instanceof SharedArrayBuffer) {
    return table;
  }

  const sharedBuffer = new SharedArrayBuffer(table.data.byteLength);
  new Uint8Array(sharedBuffer).set(
    new Uint8Array(
      table.data.buffer,
      table.data.byteOffset,
      table.data.byteLength,
    ),
  );
  return {
    ...table,
    data:
      table.dtype === "F16"
        ? new Uint16Array(sharedBuffer)
        : new Float32Array(sharedBuffer),
  };
}

export function staticEmbeddingTableFromWorkerData(
  data: Model2VecWorkerData,
): StaticEmbeddingTable {
  return {
    data:
      data.dtype === "F16"
        ? new Uint16Array(data.tableBuffer)
        : new Float32Array(data.tableBuffer),
    dimension: data.dimension,
    dtype: data.dtype,
    rows: data.rows,
  };
}

function embedStaticTokenList(
  tokenIds: ArrayLike<number>,
  table: StaticEmbeddingTable,
  normalize: boolean,
): number[] {
  const dimension = table.dimension;
  const halfValues = table.dtype === "F16" ? halfFloatValues() : null;
  const vector = Array.from({ length: dimension }, () => 0);
  if (tokenIds.length === 0) {
    return vector;
  }

  for (let index = 0; index < tokenIds.length; index++) {
    const tokenId = tokenIds[index];
    if (!Number.isInteger(tokenId) || tokenId < 0 || tokenId >= table.rows) {
      throw new Error(
        `Tokenizer returned out-of-range token id: id=${tokenId} rows=${table.rows}`,
      );
    }
    const start = tokenId * dimension;
    for (let column = 0; column < dimension; column++) {
      const value = table.data[start + column];
      vector[column] += halfValues ? halfValues[value] : value;
    }
  }

  let squaredNorm = 0;
  for (let column = 0; column < dimension; column++) {
    vector[column] /= tokenIds.length;
    squaredNorm += vector[column] * vector[column];
  }
  if (normalize && squaredNorm > 0) {
    const inverseNorm = 1 / Math.sqrt(squaredNorm);
    for (let column = 0; column < dimension; column++) {
      vector[column] *= inverseNorm;
    }
  }
  return vector;
}

function validatePackedTokenLists(packed: PackedModel2VecTokenLists): void {
  if (packed.offsets.length === 0 || packed.offsets[0] !== 0) {
    throw new Error("Model2Vec token offsets must start at zero");
  }
  for (let index = 1; index < packed.offsets.length; index++) {
    if (packed.offsets[index] < packed.offsets[index - 1]) {
      throw new Error("Model2Vec token offsets must be ordered");
    }
  }
  if (packed.offsets[packed.offsets.length - 1] !== packed.tokenIds.length) {
    throw new Error("Model2Vec token offsets do not match the token buffer");
  }
}

let cachedHalfFloatValues: Float32Array | null = null;

function halfFloatValues(): Float32Array {
  if (cachedHalfFloatValues) {
    return cachedHalfFloatValues;
  }
  const values = new Float32Array(65_536);
  for (let bits = 0; bits < values.length; bits++) {
    const sign = bits & 0x8000 ? -1 : 1;
    const exponent = (bits >> 10) & 0x1f;
    const fraction = bits & 0x03ff;
    if (exponent === 0) {
      values[bits] = sign * 2 ** -14 * (fraction / 1024);
    } else if (exponent === 0x1f) {
      values[bits] = fraction === 0 ? sign * Infinity : Number.NaN;
    } else {
      values[bits] = sign * 2 ** (exponent - 15) * (1 + fraction / 1024);
    }
  }
  cachedHalfFloatValues = values;
  return values;
}
