import { Worker } from "node:worker_threads";

// The deadline belongs to the parent thread: a timer in a blocked matcher cannot
// stop it. Fixtures are owned/cleaned by the parent even if the worker is killed.
export async function inWorker(body, data, deadlineMs = 2_000) {
  const script = `
    import { parentPort, workerData } from 'node:worker_threads';
    import assert from 'node:assert/strict';
    import * as glob from ${JSON.stringify(new URL("../../dist/engine/utils/glob.js", import.meta.url).href)};
    import { withGlobBudget } from ${JSON.stringify(new URL("../../dist/engine/utils/glob-budget.js", import.meta.url).href)};
    import { scanRootPaths } from ${JSON.stringify(new URL("../../dist/engine/pipeline/indexing/scanner/index.js", import.meta.url).href)};
    import { zvecGrepSearchInputSchema } from ${JSON.stringify(new URL("../../dist/mcp/schemas.js", import.meta.url).href)};
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
      // Include an import/startup deadline so errors before 'ready' cannot hang CI.
      timer = setTimeout(
        () => reject(new Error("Glob worker failed to start")),
        15_000,
      );
      worker.on("error", reject);
      worker.on("exit", (code) =>
        reject(new Error(`Glob worker exited early: ${code}`)),
      );
      worker.on("message", (message) => {
        if (message === "ready") {
          clearTimeout(timer);
          timer = setTimeout(
            () => reject(new Error(`Glob work exceeded ${deadlineMs} ms`)),
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
