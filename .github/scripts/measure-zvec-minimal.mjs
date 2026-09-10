import assert from "node:assert/strict";
import { appendFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import {
  ZVecInitialize,
  ZVecLogLevel,
  ZVecCreateAndOpen,
  ZVecCollectionSchema,
  ZVecDataType,
  ZVecIndexType,
} from "@zvec/zvec";

ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
const output = process.env.PHASE_OUTPUT ?? "minimal-results.jsonl";
for (let iteration = 1; iteration <= 5; iteration++) {
  // Alternate order to avoid always giving one configuration a warm cache.
  const modes = iteration % 2 ? [0, 1] : [1, 0];
  for (const concurrency of modes) {
    const directory = await mkdtemp(join(tmpdir(), "zvec-minimal-"));
    const records = [];
    function measure(name, operation) {
      const start = performance.now();
      const cpu = process.cpuUsage();
      try {
        return operation();
      } finally {
        const usage = process.cpuUsage(cpu);
        records.push({
          iteration,
          concurrency,
          name,
          ms: performance.now() - start,
          cpuMs: (usage.user + usage.system) / 1000,
        });
      }
    }
    let collection;
    try {
      collection = measure("create", () =>
        ZVecCreateAndOpen(
          join(directory, "collection"),
          new ZVecCollectionSchema({
            name: "minimal",
            fields: [
              {
                name: "file_id",
                dataType: ZVecDataType.STRING,
                nullable: false,
                indexParams: { indexType: ZVecIndexType.INVERT },
              },
            ],
          }),
        ),
      );
      const status = measure("upsert", () =>
        collection.upsertSync({ id: "one", fields: { file_id: "alpha.ts" } }),
      );
      assert.equal(status.ok, true);
      measure("optimize", () => collection.optimizeSync({ concurrency }));
      const found = measure("fetch", () =>
        collection.fetchSync({ ids: ["one"] }),
      );
      assert.equal(found.one.fields.file_id, "alpha.ts");
    } finally {
      if (collection) measure("close", () => collection.closeSync());
      const start = performance.now();
      await rm(directory, { recursive: true, force: true });
      records.push({
        iteration,
        concurrency,
        name: "cleanup",
        ms: performance.now() - start,
      });
      for (const record of records) {
        const line = JSON.stringify(record);
        appendFileSync(output, line + "\n");
        console.log(line);
      }
    }
  }
}
