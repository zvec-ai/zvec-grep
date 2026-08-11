import { parentPort, workerData } from "node:worker_threads";
import {
  embedPackedModel2VecTokenLists,
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
const table = staticEmbeddingTableFromWorkerData(data);

port.postMessage({ type: "ready" } satisfies Model2VecWorkerResponse);
port.on("message", async (request: Model2VecWorkerRequest) => {
  try {
    const vectors = embedPackedModel2VecTokenLists(
      {
        tokenIds: new Int32Array(request.tokenIds),
        offsets: new Uint32Array(request.offsets),
      },
      table,
      data.normalize,
    );
    const flatVectors = new Float32Array(vectors.length * data.dimension);
    for (const [index, vector] of vectors.entries()) {
      flatVectors.set(vector, index * data.dimension);
    }
    const response: Model2VecWorkerResponse = {
      type: "result",
      id: request.id,
      vectors: flatVectors.buffer,
      vectorCount: vectors.length,
      truncated: [],
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
