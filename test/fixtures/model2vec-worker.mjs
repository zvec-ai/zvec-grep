import { threadId, parentPort } from "node:worker_threads";

parentPort.postMessage({ type: "ready" });
parentPort.on("message", ({ id, texts }) => {
  if (texts[0] === "error") {
    parentPort.postMessage({
      type: "error",
      id,
      error: { name: "FixtureError", message: "fixture worker failed" },
    });
    return;
  }
  if (texts[0] === "malformed") {
    parentPort.postMessage({
      type: "result",
      id,
      vectors: new ArrayBuffer(0),
      vectorCount: 1,
      truncated: [],
    });
    return;
  }
  const delayMs = Number.parseInt(texts[0] ?? "0", 10) || 0;
  setTimeout(() => {
    const vectorCount = texts.length;
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
