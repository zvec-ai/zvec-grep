import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { productCandidate, candidateCommand } from "../engines/zg/run.mjs";
import { parseNativeStatus } from "../engines/zg/snapshot.mjs";
import {
  performanceReport,
  pairedCell,
  validatePerformanceCoverage,
} from "../self-hosted/report.mjs";

test("Node release adapter changes CLI syntax without changing V8 execution policy", () => {
  const candidate = productCandidate("/tmp/release", "/tmp/client", {
    name: "@zvec/zvec-grep",
    version: "0.2.2",
    bin: { zg: "dist/cli/index.js" },
  });
  assert.equal(candidate.runtime, "node-0.2.2");
  assert.deepEqual(
    candidateCommand(candidate, [
      "--index",
      "/tmp/corpus",
      "--embedding-concurrency",
      "4",
    ]),
    {
      command: process.execPath,
      args: [
        "/tmp/release/dist/cli/index.js",
        "index",
        "/tmp/corpus",
        "--embedding-concurrency",
        "4",
      ],
    },
  );
  assert.throws(
    () =>
      productCandidate("/tmp/release", "/tmp/client", {
        name: "@zvec/zvec-grep",
        version: "0.2.3",
        bin: { zg: "index.js" },
      }),
    /unchanged 0.2.2/,
  );
});

test("Node status allows only its absent source byte field, retaining readiness checks", () => {
  const text = `✓ Workspace index is ready
  /tmp/corpus
  Coverage    #### 100%  8 / 8 files
  Entities    16
  Queue       0 pending · 0 failed
  Embedding   local/potion-code-16m-v2
  Storage     .zvec-grep
`;
  assert.equal(
    parseNativeStatus(text, { runtime: "node-0.2.2" }).indexed_source_bytes,
    null,
  );
  assert.throws(() => parseNativeStatus(text), /source size/);
  assert.throws(
    () =>
      parseNativeStatus(text.replace("0 pending", "1 pending"), {
        runtime: "node-0.2.2",
      }),
    /pending/,
  );
});

test("profile report covers four suites, all query modes and all five performance sections", () => {
  assert.throws(() => validatePerformanceCoverage({}), /missing index groups/);
  const report = performanceReport(
    { node: { reports: {} }, rust: { reports: {} } },
    {
      node: { version: "0.2.2" },
      rust: { version: "0.3.0-rc.3" },
      hardware: { memory_bytes: 1024 ** 3 },
    },
    { status: "failed", errors: ["missing reports"] },
  );
  for (const title of [
    "1. Index time",
    "2. Query request RT",
    "3. Query group time",
    "4. Index / Query RSS",
    "5. Index size",
    "Index size distribution",
  ])
    assert.ok(report.includes(title));
  for (const suite of ["SWE-QA20", "BEIR", "DuRetrieval", "Quarry"])
    assert.ok(report.includes(`| ${suite} | hybrid |`));
  assert.ok(report.includes("Incomplete or invalid run"));
  assert.equal(pairedCell(1, 0.5), "1.00 / 0.50 (-50.0%)");
  assert.equal(pairedCell(null, 2), "—");
  assert.equal(pairedCell(0, 2), "0.00 / 2.00 (N/A)");
});

test("persistent runner is selected only for a maintainer manual npm comparison", async () => {
  const parent = await readFile(
    new URL("../../../.github/workflows/retrieval-only.yml", import.meta.url),
    "utf8",
  );
  const child = await readFile(
    new URL(
      "../../../.github/workflows/retrieval-npm-self-hosted.yml",
      import.meta.url,
    ),
    "utf8",
  );
  assert.match(
    parent,
    /if: github.event_name == 'workflow_dispatch' && inputs.benchmark_target == 'node022-rust-canary'/,
  );
  assert.match(child, /runs-on: \[zvec-retrieval-benchmark\]/);
  assert.match(child, /retrieval-authorize/);
  assert.match(child, /cancel-in-progress: false/);
  assert.match(child, /flock -n/);
  assert.doesNotMatch(child, /pull_request:|secrets: inherit/);
});
