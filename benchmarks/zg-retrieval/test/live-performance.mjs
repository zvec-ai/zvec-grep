// Explicit live integration test: real immutable Node 0.2.2 and a pinned Rust package.
// This creates an isolated synthetic corpus; it is not a quality benchmark result.
import assert from "node:assert/strict";
import { mkdir, mkdtemp, realpath, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { pathToFileURL } from "node:url";
import { corpusManifest } from "../core/corpus.mjs";
import { run, writeJson } from "../core/io.mjs";
import { packageCandidate } from "../engines/zg/run.mjs";
import { runGroup } from "../expansion/run.mjs";
import {
  markdownPerformance,
  summarizePerformance,
} from "../reports/performance.mjs";

const { values } = parseArgs({
  options: {
    "node-package": { type: "string" },
    "rust-package": { type: "string" },
    "rust-commit": { type: "string" },
    output: { type: "string" },
    "model-cache": { type: "string" },
  },
});
for (const name of ["node-package", "rust-package", "rust-commit", "output"])
  assert.ok(values[name], `requires --${name}`);
assert.match(values["rust-commit"], /^[a-f0-9]{40}$/);
const output = resolve(values.output);
await mkdir(output, { recursive: false });
const modelCache = resolve(
  values["model-cache"] ?? join(output, "model-cache"),
);
await mkdir(modelCache, { recursive: true });
const results = [];
for (const [arm, packagePath] of [
  ["node", values["node-package"]],
  ["rust", values["rust-package"]],
]) {
  const armOutput = join(output, arm);
  await mkdir(armOutput);
  const candidate = await packageCandidate(packagePath, armOutput);
  assert.equal(
    candidate.runtime,
    arm === "node" ? "node-0.2.2" : "rust-native",
  );
  // Node 0.2.2 discovers ancestor indexes. Keep fixtures outside any existing
  // checkout/index so the test cannot open or modify the user's workspace index.
  const root = await realpath(
    await mkdtemp(join(tmpdir(), `zg-performance-${arm}-`)),
  );
  for (let index = 0; index < 80; index++)
    await writeFile(
      join(root, `route_${index}.py`),
      `"""Handles route_${index} requests and validates their input."""\ndef route_${index}(value):\n    return value + ${index}\n`,
    );
  await run("git", ["init", "--quiet", root]);
  await run("git", [
    "-C",
    root,
    "-c",
    "core.hooksPath=/dev/null",
    "add",
    "--",
    "*.py",
  ]);
  const corpus = await corpusManifest(root);
  assert.equal(corpus.entries.length, 80);
  if (results.length) assert.equal(corpus.sha256, results[0].corpus_sha256);
  const tasks = Array.from({ length: 20 }, (_, index) => ({
    id: `query-${index}`,
    query: `route_${index}`,
    targets: [{ path: `route_${index}.py` }],
  }));
  const require = createRequire(join(candidate.consumer, "package.json"));
  const { Client } = await import(
    pathToFileURL(require.resolve("@modelcontextprotocol/client")).href
  );
  const { StdioClientTransport } = await import(
    pathToFileURL(require.resolve("@modelcontextprotocol/client/stdio")).href
  );
  let active = 0,
    peak = 0;
  class ObservedClient extends Client {
    async callTool(...args) {
      peak = Math.max(peak, ++active);
      try {
        return await super.callTool(...args);
      } finally {
        active--;
      }
    }
  }
  const report = {
    rows: [],
    failures: [],
    index_seconds: {},
    performance_groups: [],
  };
  let failure;
  try {
    await runGroup(
      {
        name: "live-performance",
        model: "local/potion-code-16m-v2",
        modes: ["hybrid", "fts", "vector"],
        config: { indexTimeoutMinutes: 10 },
      },
      {
        id: "fixture",
        root,
        tasks,
        indexGlob: "*.py",
        expectedIndexedFiles: 80,
      },
      candidate,
      { output: armOutput, modelCache },
      report,
      { Client: ObservedClient, StdioClientTransport },
    );
    assert.equal(report.rows.length, 60);
    assert.ok(report.rows.every((row) => row.status === "success"));
    assert.equal(report.failures.length, 0);
    assert.equal(active, 0);
    assert.equal(peak, 4);
    const measured = report.performance_groups[0];
    assert.ok(measured.index.index_bytes > 0);
    assert.equal(measured.index.memory.status, "available");
    assert.equal(Object.hasOwn(measured.index, "write"), false);
    for (const mode of ["hybrid", "fts", "vector"]) {
      assert.equal(measured.query[mode].samples_ms.length, 100);
      assert.equal(measured.query[mode].memory.status, "available");
      assert.ok(measured.query[mode].total_ms > 0);
    }
  } catch (error) {
    failure = error;
  }
  const result = {
    arm,
    runtime: candidate.runtime,
    package: candidate.identity,
    candidate_commit: arm === "rust" ? values["rust-commit"] : null,
    corpus_sha256: corpus.sha256,
    corpus_root: root,
    model: "local/potion-code-16m-v2",
    max_in_flight: peak,
    status: failure ? "failed" : "passed",
    error: failure?.stack ?? null,
    performance: summarizePerformance(report.performance_groups),
    report,
  };
  await writeJson(join(armOutput, "integration.json"), result);
  results.push(result);
}
await writeJson(join(output, "integration.json"), {
  kind: "live-integration-test-not-quality-benchmark",
  results,
});
await writeFile(
  join(output, "integration.md"),
  [
    "# Live performance integration test",
    "",
    "Synthetic 80-file corpus; 20 queries × 3 modes × 5 repetitions per runtime. These are integration checks, not full-suite performance conclusions.",
    "",
    ...results.flatMap((result) => [
      `## ${result.runtime}: ${result.status}`,
      "",
      `Package SHA256: \`${result.package.tarball_sha256}\`. Rust commit: \`${result.candidate_commit ?? "not applicable"}\`. Observed maximum in-flight requests: ${result.max_in_flight}.`,
      "",
      markdownPerformance(result.performance),
      ...(result.error ? ["```", result.error, "```"] : []),
    ]),
  ].join("\n"),
);
console.log(`Live test: ${join(output, "integration.md")}`);
assert.ok(
  results.every((result) => result.status === "passed"),
  "a real-product integration arm failed; inspect integration.json",
);
