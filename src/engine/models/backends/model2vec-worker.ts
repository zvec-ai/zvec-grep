import { parentPort, workerData } from "node:worker_threads";
import { loadModel2VecTokenizer } from "./model2vec-tokenizer.js";
import {
  embedModel2VecTexts,
  staticEmbeddingTableFromWorkerData,
  type Model2VecWorkerData,
  type Model2VecWorkerRequest,
  type Model2VecWorkerResponse,
} from "./model2vec-runtime.js";

const port = parentPort;
if (!port) {
  throw new Error("Model2Vec worker requires a parent port");
}

const data = workerData as Model2VecWorkerData;
const tokenizer = await loadModel2VecTokenizer(data.tokenizerSource);
const table = staticEmbeddingTableFromWorkerData(data);

port.postMessage({ type: "ready" } satisfies Model2VecWorkerResponse);
port.on("message", async (request: Model2VecWorkerRequest) => {
  try {
    const result = await embedModel2VecTexts(
      request.texts,
      tokenizer,
      table,
      data.maxInputTokens,
      data.normalize,
    );
    const flatVectors = new Float32Array(
      result.vectors.length * data.dimension,
    );
    for (const [index, vector] of result.vectors.entries()) {
      flatVectors.set(vector, index * data.dimension);
    }
    const response: Model2VecWorkerResponse = {
      type: "result",
      id: request.id,
      vectors: flatVectors.buffer,
      vectorCount: result.vectors.length,
      truncated: result.truncated,
    };
    port.postMessage(response, [flatVectors.buffer]);
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
    } satisfies Model2VecWorkerResponse);
  }
});
