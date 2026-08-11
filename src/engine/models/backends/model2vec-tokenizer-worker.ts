import { parentPort, workerData } from "node:worker_threads";
import { loadModel2VecTokenizer } from "./model2vec-tokenizer.js";
import {
  packModel2VecTokenLists,
  tokenizeModel2VecTexts,
  type Model2VecTokenizerWorkerRequest,
  type Model2VecTokenizerWorkerResponse,
  type Model2VecWorkerData,
} from "./model2vec-runtime.js";

const port = parentPort;
if (!port) {
  throw new Error("Model2Vec tokenizer worker requires a parent port");
}

const data = workerData as Model2VecWorkerData;
const tokenizer = await loadModel2VecTokenizer(data.tokenizerSource);

port.postMessage({ type: "ready" } satisfies Model2VecTokenizerWorkerResponse);
port.on("message", async (request: Model2VecTokenizerWorkerRequest) => {
  try {
    const tokenized = await tokenizeModel2VecTexts(
      request.texts,
      tokenizer,
      data.maxInputTokens,
    );
    const packed = packModel2VecTokenLists(tokenized.tokenLists);
    const tokenIds = packed.tokenIds.buffer as ArrayBuffer;
    const offsets = packed.offsets.buffer as ArrayBuffer;
    const response: Model2VecTokenizerWorkerResponse = {
      type: "tokenized",
      id: request.id,
      tokenIds,
      offsets,
      truncated: tokenized.truncated,
    };
    port.postMessage(response, [tokenIds, offsets]);
  } catch (error) {
    const cause = error instanceof Error ? error : new Error(String(error));
    port.postMessage({
      type: "error",
      id: request.id,
      error: {
        name: cause.name,
        message: cause.message,
        stack: cause.stack,
      },
    } satisfies Model2VecTokenizerWorkerResponse);
  }
});
