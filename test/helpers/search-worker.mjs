import { Worker } from "node:worker_threads";

// Parent-supervised deadline for search workloads: the timer lives in the
// parent thread, so a blocked worker loop cannot silence it. Fixtures and
// timers inside the worker are owned by the worker body; the parent kills the
// worker on deadline and rejects.
export async function inSearchWorker(body, data, deadlineMs = 120_000) {
  const script = `
    import { parentPort, workerData } from 'node:worker_threads';
    import assert from 'node:assert/strict';
    import { searchWorkspaceIndex as search } from ${JSON.stringify(new URL("../../dist/engine/pipeline/search/index.js", import.meta.url).href)};
    import { FakeEmbeddingModel } from ${JSON.stringify(new URL("./fake-embedding.mjs", import.meta.url).href)};
    parentPort.postMessage('ready');
    ${body}
    parentPort.postMessage('done');
  `;
  const worker = new Worker(
    new URL(`data:text/javascript,${encodeURIComponent(script)}`),
    { workerData: data },
  );
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(
        () => reject(new Error("Search worker failed to start")),
        15_000,
      );
      worker.on("error", reject);
      worker.on("exit", (code) =>
        reject(new Error(`Search worker exited early: ${code}`)),
      );
      worker.on("message", (message) => {
        if (message === "ready") {
          clearTimeout(timer);
          timer = setTimeout(
            () => reject(new Error(`Search work exceeded ${deadlineMs} ms`)),
            deadlineMs,
          );
        } else if (message === "done") resolve();
      });
    });
  } finally {
    clearTimeout(timer);
    await worker.terminate();
  }
}
