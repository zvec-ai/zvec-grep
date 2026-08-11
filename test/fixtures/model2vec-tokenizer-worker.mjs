import { parentPort } from "node:worker_threads";

parentPort.postMessage({ type: "ready" });
parentPort.on("message", ({ id, texts }) => {
  const tokenIds = Int32Array.from(texts, (text) =>
    text === "error"
      ? -1
      : text === "malformed"
        ? -2
        : Number.parseInt(text, 10) || 0,
  );
  const offsets = Uint32Array.from(
    { length: texts.length + 1 },
    (_, index) => index,
  );
  parentPort.postMessage(
    {
      type: "tokenized",
      id,
      tokenIds: tokenIds.buffer,
      offsets: offsets.buffer,
      truncated: [],
    },
    [tokenIds.buffer, offsets.buffer],
  );
});
