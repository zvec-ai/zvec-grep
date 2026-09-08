import assert from "node:assert/strict";
import { createEmbeddingModel } from "../../dist/engine/models/index.js";

const reference = process.argv[2];
assert.ok(
  reference?.startsWith("local/"),
  "Only local model references are allowed",
);
assert.ok(
  process.env.ZVEC_GREP_MODEL_CACHE,
  "Set an explicit evaluation model cache",
);
const model = createEmbeddingModel(reference, {
  modelCacheDir: process.env.ZVEC_GREP_MODEL_CACHE,
  device: "cpu",
});
let reportedAt = 0;
try {
  const start = performance.now();
  const result = await model.embed(
    [{ kind: "text", text: "后台服务 access token validation" }],
    {
      purpose: "query",
      onProgress: (progress) => {
        if (
          progress.stage !== "downloading" ||
          Date.now() - reportedAt >= 5_000
        ) {
          console.log(JSON.stringify({ type: "progress", ...progress }));
          reportedAt = Date.now();
        }
      },
    },
  );
  const preparedMs = Math.round(performance.now() - start);
  const warmStart = performance.now();
  await model.embed(
    [{ kind: "text", text: "reuse a database read handle across searches" }],
    { purpose: "query" },
  );
  console.log(
    JSON.stringify({
      type: "prepared",
      reference,
      dimension: result.vectors[0].length,
      preparedMs,
      warmMs: Math.round(performance.now() - warmStart),
      peakRssKiB: process.resourceUsage().maxRSS,
    }),
  );
} finally {
  await model.dispose();
}
