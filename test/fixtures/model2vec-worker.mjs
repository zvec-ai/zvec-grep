import { threadId, parentPort } from "node:worker_threads";

parentPort.postMessage({ type: "ready" });
parentPort.on("message", ({ id, tokenIds, offsets }) => {
  const tokens = new Int32Array(tokenIds);
  const tokenOffsets = new Uint32Array(offsets);
  if (tokens[0] === -1) {
    parentPort.postMessage({
      type: "error",
      id,
      error: { name: "FixtureError", message: "fixture worker failed" },
    });
    return;
  }
  if (tokens[0] === -2) {
    parentPort.postMessage({
      type: "result",
      id,
      vectors: new ArrayBuffer(0),
      vectorCount: 1,
      truncated: [],
    });
    return;
  }
  const delayMs = tokens[0] || 0;
  setTimeout(() => {
    const vectorCount = tokenOffsets.length - 1;
    const vectors = new Float32Array(vectorCount * 2).fill(threadId);
    parentPort.postMessage(
      {
        type: "result",
        id,
        vectors: vectors.buffer,
        vectorCount,
        truncated: [],
      },
      [vectors.buffer],
    );
  }, delayMs);
});
