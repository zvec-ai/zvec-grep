import assert from "node:assert/strict";
import { readdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve, isAbsolute } from "node:path";
import {
  loadSuite,
  readJson,
  writeJson,
  objectHash,
  inside,
  fileHash,
} from "../../core/lib.mjs";
import { scoreResponse, validateSearchRoute } from "./parse.mjs";
import { scoreNdcg } from "../../metrics/ndcg.mjs";
import {
  FILE_RETRIEVAL_CONTRACT,
  fileRetrievalForRow,
  summarizeFileRetrieval,
} from "../../metrics/files.mjs";

import {
  summarizeMeasurements,
  validateCallLatency,
} from "../../metrics/measurements.mjs";

import { summarizeNdcg } from "../../metrics/summary.mjs";
import {
  rankedLocations,
  summarizeRepeatedQuality,
} from "../../metrics/repetitions.mjs";
import { markdownReport } from "../../reports/zg.mjs";

async function findRuns(directory) {
  const runs = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.isFile() && entry.name === "run.json") runs.push(directory);
    else if (
      entry.isDirectory() &&
      ![
        "consumer",
        "node_modules",
        "runtime-home",
        "model-cache",
        "raw",
        "stages",
        "installation",
      ].includes(entry.name)
    )
      runs.push(...(await findRuns(join(directory, entry.name))));
  }
  return runs;
}

function invalidate(score, reason) {
  return {
    ...score,
    status: "harness_invalid",
    execution_status: "harness_invalid",
    invalid_reason: reason,
    file_retrieval: null,
    ndcg: null,
    quality_mean: null,
  };
}

export async function aggregate(directory, { expectedTasks } = {}) {
  directory = resolve(directory);
  const suite = await loadSuite();
  const expected =
    expectedTasks ?? suite.lock.tasks.map((task) => task.task_id);
  const errors = [],
    observations = [],
    manifests = [];
  const runDirectories = await findRuns(directory);
  const seen = new Set();
  for (const runDirectory of runDirectories) {
    const manifest = await readJson(join(runDirectory, "run.json"));
    manifests.push(manifest);
    const invalid = [...(manifest.invalid_reasons ?? [])];
    if (objectHash(manifest.suite) !== objectHash(suite.identity))
      invalid.push("suite/protocol/gold hash differs from the scorer checkout");
    if (objectHash(manifest.protocol) !== objectHash(suite.protocol))
      invalid.push("protocol content differs from the frozen suite");
    const repo = suite.lock.repositories.find(
      (repo) => repo.repository === manifest.repository,
    );
    if (!repo || manifest.repository_commit !== repo.commit)
      invalid.push("repository identity mismatch");
    if (!manifest.finished_at) invalid.push("run did not finish");
    let calls = [];
    try {
      calls = (await readFile(join(runDirectory, "requests.jsonl"), "utf8"))
        .split("\n")
        .filter(Boolean)
        .map(JSON.parse);
    } catch (error) {
      invalid.push(`missing/invalid call log: ${error.message}`);
    }
    // A completed call log alone is not proof of a valid frozen experiment.
    if (manifest.preparation_status === "ready") {
      try {
        assert.equal(
          manifest.post_run_integrity,
          "verified",
          "post-run integrity not verified",
        );
        for (const phase of ["before", "after"]) {
          const snapshot = await readJson(
            join(runDirectory, `stages/${phase}/summary.json`),
          );
          assert.equal(
            snapshot.schema_version,
            2,
            "unsupported native snapshot",
          );
          assert.equal(snapshot.kind, "rust-public-status");
          assert.equal(
            snapshot.logical_content_sha256,
            manifest.index_content_sha256,
            "index identity drift",
          );
          assert.ok(
            snapshot.artifacts &&
              ["status.txt", "status.json"].every((path) =>
                Object.hasOwn(snapshot.artifacts, path),
              ),
            "missing snapshot artifact identities",
          );
          for (const [path, hash] of Object.entries(snapshot.artifacts)) {
            assert.ok(
              ["status.txt", "status.stderr.txt", "status.json"].includes(path),
            );
            assert.equal(
              await fileHash(join(runDirectory, `stages/${phase}`, path)),
              hash,
              `snapshot artifact drift: ${path}`,
            );
          }
          const status = await readJson(
            join(runDirectory, `stages/${phase}/status.json`),
          );
          assert.ok(
            Number.isSafeInteger(status.files_indexed) &&
              status.files_indexed > 0 &&
              status.files_pending === 0 &&
              status.files_failed === 0,
            "native public status does not describe a ready index",
          );
        }
        const selection = suite.protocol.index_selection;
        assert.deepEqual(
          manifest.index_selection_audit,
          {
            content: "code",
            max_file_size_bytes: selection.max_file_size_bytes,
            requested_extensions: selection.code_extensions.length,
            verified: "request_arguments_and_public_status",
          },
          "index selection audit differs from the frozen request",
        );
        for (const [path, hash] of [
          ["corpus.json", manifest.corpus_sha256],
          ["corpus-after.json", manifest.corpus_sha256],
          ["model-files.json", manifest.model_files_sha256],
          ["model-files-after.json", manifest.model_files_sha256],
        ]) {
          const inventory = await readJson(join(runDirectory, path));
          assert.match(hash, /^[a-f0-9]{64}$/);
          assert.equal(inventory.sha256, hash, `${path}: identity mismatch`);
          assert.equal(
            objectHash(inventory.entries),
            hash,
            `${path}: truncated/changed inventory`,
          );
        }
      } catch (error) {
        invalid.push(`frozen experiment audit: ${error.message}`);
      }
    } else if (
      !manifest.preparation_error ||
      calls.some((call) => !call.preparation_error)
    ) {
      invalid.push(
        "preparation did not succeed and no complete product preparation failure was recorded",
      );
    }
    const modeSet = suite.protocol.modes;
    if (objectHash(manifest.modes ?? null) !== objectHash(modeSet))
      invalid.push("mode selection differs from frozen three-mode protocol");
    if (manifest.preview !== suite.protocol.preview)
      invalid.push("preview selection differs from frozen protocol");
    const plannedCount =
      manifest.tasks.length * modeSet.length * suite.protocol.repetitions;
    if (
      calls.length !== plannedCount ||
      manifest.planned_calls !== plannedCount
    )
      invalid.push("incomplete planned call matrix");
    if (
      manifest.tasks.some(
        (id) =>
          !expected.includes(id) ||
          !suite.lock.tasks.some(
            (task) =>
              task.task_id === id && task.repository === manifest.repository,
          ),
      )
    )
      invalid.push("unexpected task selection");
    if (
      objectHash(manifest.tasks) !==
      objectHash(
        suite.lock.tasks
          .filter(
            (task) =>
              task.repository === manifest.repository &&
              expected.includes(task.task_id),
          )
          .map((task) => task.task_id),
      )
    )
      invalid.push(
        "repository task coverage/order differs from frozen selection",
      );
    const plannedOrder = modeSet.flatMap((mode) =>
      manifest.tasks.flatMap((task_id) =>
        Array.from({ length: suite.protocol.repetitions }, (_, index) => ({
          task_id,
          mode,
          preview: suite.protocol.preview,
          repetition: index + 1,
        })),
      ),
    );
    if (
      objectHash(
        calls.map(({ task_id, mode, preview, repetition }) => ({
          task_id,
          mode,
          preview,
          repetition,
        })),
      ) !== objectHash(plannedOrder)
    )
      invalid.push("call order differs from mode-task-repetition protocol");
    for (const call of calls) {
      const task = suite.lock.tasks.find(
        (task) => task.task_id === call.task_id,
      );
      if (!task || !manifest.tasks.includes(call.task_id)) {
        errors.push(`unknown/unplanned task ${call.task_id}`);
        continue;
      }
      const key = `${call.task_id}/${call.mode}/${call.preview}/${call.repetition}`;
      if (seen.has(key)) {
        errors.push(`duplicate call: ${key}`);
        continue;
      }
      seen.add(key);
      const callInvalid = [...invalid];
      if (
        !modeSet.includes(call.mode) ||
        call.preview !== suite.protocol.preview ||
        !Number.isInteger(call.repetition) ||
        call.repetition < 1 ||
        call.repetition > suite.protocol.repetitions
      )
        callInvalid.push("unplanned mode/preview/repetition");
      const args = call.request?.arguments;
      const expectedRoot = manifest.corpus_root ?? "<unavailable>";
      if (expectedRoot !== "<unavailable>" && !isAbsolute(expectedRoot))
        callInvalid.push("non-absolute corpus root");
      if (expectedRoot === "<unavailable>" && !call.preparation_error)
        callInvalid.push("missing corpus root");
      const expectedArgs = {
        root: expectedRoot,
        ...(call.mode === "hybrid"
          ? { query: task.query }
          : { [call.mode]: [task.query] }),
        limit: suite.protocol.limit,
        ...suite.protocol.request,
      };
      if (
        call.request?.name !== "zvec_grep_search" ||
        objectHash(args) !== objectHash(expectedArgs)
      )
        callInvalid.push("request does not match the original-query protocol");
      if (
        call.quality_observation !==
        (call.repetition === suite.protocol.quality_repetition)
      )
        callInvalid.push("incorrect quality repetition");
      if (call.harness_error) callInvalid.push(call.harness_error);
      let score;
      let visibleOutputBytes = null;
      try {
        assert.equal(
          call.raw_path,
          `raw/${task.task_slug}-${call.mode}-${call.preview}-${call.repetition}.json`,
          "raw response reused or mapped to the wrong call",
        );
        const path = join(runDirectory, call.raw_path);
        assert.ok(inside(runDirectory, path), "raw response path escapes run");
        assert.equal(
          await fileHash(path),
          call.raw_sha256,
          "raw response changed since capture",
        );
        const response = await readJson(path);
        if (
          Array.isArray(response?.content) &&
          response.content.every(
            (block) => block.type === "text" && typeof block.text === "string",
          )
        )
          visibleOutputBytes = Buffer.byteLength(
            response.content.map((block) => block.text).join("\n"),
            "utf8",
          );
        score = scoreResponse(response, suite.gold[call.task_id]);
        if (score.execution_status === "success")
          validateSearchRoute(score.items, call.mode);
      } catch (error) {
        callInvalid.push(error.message);
        score = {};
      }
      try {
        validateCallLatency({ ...call, ...score });
      } catch (error) {
        callInvalid.push(error.message);
      }
      if (callInvalid.length) score = invalidate(score, callInvalid.join("; "));
      observations.push({
        ...call,
        ...score,
        visible_output_bytes: visibleOutputBytes,
        language: suite.file_gold[call.task_id].language,
        ndcg:
          score.status === "harness_invalid"
            ? null
            : {
                targets: suite.file_gold[call.task_id].targets,
                ...scoreNdcg(
                  score.items ?? [],
                  suite.file_gold[call.task_id].targets,
                ),
              },
        category: task.category,
        repository: task.repository,
        raw_path: `${runDirectory.slice(directory.length + 1)}/${call.raw_path}`,
      });
    }
    for (const taskId of manifest.tasks)
      for (const mode of modeSet)
        for (
          let repetition = 1;
          repetition <= suite.protocol.repetitions;
          repetition++
        )
          if (
            !seen.has(
              `${taskId}/${mode}/${suite.protocol.preview}/${repetition}`,
            )
          )
            errors.push(
              `missing call: ${taskId}/${mode}/${suite.protocol.preview}/${repetition}`,
            );
    errors.push(
      ...invalid.map((reason) => `${manifest.repository}: ${reason}`),
    );
  }
  for (const id of expected)
    for (const mode of suite.protocol.modes)
      if (
        !observations.some(
          (row) =>
            row.task_id === id &&
            row.mode === mode &&
            row.preview === suite.protocol.preview &&
            row.repetition === suite.protocol.quality_repetition,
        )
      )
        errors.push(`missing quality observation: ${id}/${mode}`);
  for (const field of ["tarball_sha256"])
    if (
      new Set(manifests.map((manifest) => manifest.package?.[field])).size !== 1
    )
      errors.push(`mixed candidate ${field}`);
  if (
    new Set(
      manifests
        .filter((manifest) => manifest.model_files_sha256)
        .map((manifest) => manifest.model_files_sha256),
    ).size > 1
  )
    errors.push("mixed model artifact hashes across repositories");
  if (
    new Set(manifests.map((manifest) => JSON.stringify(manifest.modes))).size >
    1
  )
    errors.push("mixed mode selections across repository shards");
  if (observations.some((row) => row.status === "harness_invalid"))
    errors.push("one or more observations are experimentally invalid");
  for (const row of observations) row.file_retrieval = fileRetrievalForRow(row);
  const quality = observations
    .filter((row) => row.repetition === suite.protocol.quality_repetition)
    .map((row) => {
      const repeats = observations.filter(
        (item) =>
          item.task_id === row.task_id &&
          item.mode === row.mode &&
          item.preview === row.preview,
      );
      const valid =
        repeats.length === suite.protocol.repetitions &&
        repeats.every(
          (item) =>
            item.execution_status === "success" &&
            item.status !== "harness_invalid",
        );
      const invalidRepeat = repeats.find(
        (item) => item.status === "harness_invalid",
      );
      // A bad repetition invalidates the task/mode experiment, even when its
      // representative fifth call happened to be parseable.
      const qualityRow =
        invalidRepeat && row.status !== "harness_invalid"
          ? invalidate(
              row,
              `repetition ${invalidRepeat.repetition}: ${invalidRepeat.invalid_reason}`,
            )
          : row;
      const qualityMean =
        invalidRepeat ||
        repeats.length !== suite.protocol.repetitions ||
        row.gold_status !== "reviewed"
          ? null
          : summarizeRepeatedQuality(
              repeats,
              suite.file_gold[row.task_id].targets,
            );
      return {
        ...qualityRow,
        quality_mean: qualityMean,
        ranking_repeatable: valid
          ? (qualityMean?.ranking_repeatable ?? null)
          : null,
        output_repeatable: valid
          ? new Set(repeats.map((item) => item.visible_output_sha256)).size ===
            1
          : null,
        freshness_values: repeats.map((item) => item.freshness ?? null),
        quality_observations: repeats.map((item) => ({
          repetition: item.repetition,
          items: rankedLocations(item.items ?? []),
        })),
        measurement_observations: repeats.map((item) => ({
          repetition: item.repetition,
          status: item.status,
          execution_status: item.execution_status,
          latency_ms: item.latency_ms,
          visible_output_bytes: item.visible_output_bytes,
        })),
      };
    });
  const productErrors = observations.filter(
    (row) => row.execution_status === "product_error",
  ).length;
  const complete = errors.length === 0;
  const invalidObservations = observations.filter(
    (row) => row.status === "harness_invalid",
  );
  const partialScoreable =
    errors.every(
      (reason) =>
        reason === "one or more observations are experimentally invalid",
    ) &&
    invalidObservations.every((row) =>
      row.invalid_reason?.startsWith("format_unknown: "),
    );
  const validQuality = quality.filter(
    (row) => row.status !== "harness_invalid",
  );
  const validKeys = new Set(
    validQuality.map((row) => `${row.task_id}/${row.mode}`),
  );
  const modeReports = Object.fromEntries(
    suite.protocol.modes.map((mode) => {
      const rows = validQuality.filter((row) => row.mode === mode);
      return [
        mode,
        {
          measurements:
            partialScoreable && rows.length
              ? summarizeMeasurements(
                  observations.filter(
                    (row) =>
                      row.mode === mode &&
                      validKeys.has(`${row.task_id}/${row.mode}`),
                  ),
                )
              : null,
          file_retrieval:
            partialScoreable && rows.length
              ? summarizeFileRetrieval(rows)
              : null,
          ndcg: partialScoreable && rows.length ? summarizeNdcg(rows) : null,
          ranking_repeatable_tasks: rows.filter(
            (row) => row.ranking_repeatable === true,
          ).length,
          output_repeatable_tasks: rows.filter(
            (row) => row.output_repeatable === true,
          ).length,
        },
      ];
    }),
  );
  const report = {
    schema_version: 7,
    file_retrieval_contract: FILE_RETRIEVAL_CONTRACT,
    preview: suite.protocol.preview,
    quality_repetition: suite.protocol.quality_repetition,
    quality_aggregation: "mean_of_five",
    generated_at: new Date().toISOString(),
    suite: suite.identity,
    scope:
      expected.length === 20 ? "full-20-original-queries" : "explicit-subset",
    expected_task_ids: expected,
    observed_calls: observations.length,
    integrity_passed: complete && productErrors === 0,
    quality_score_valid: complete,
    integrity_errors: [...new Set(errors)],
    product_error_calls: productErrors,
    quality_gate: "report-only; no arbitrary quality threshold",
    aggregation:
      "each query/mode averages all five repetition scores; file Hit@1/5/10 and MRR@10 weight original questions equally; nDCG@10 weights repositories equally after averaging questions within each repository; all five metrics use frozen accepted-file targets and native ranks",
    modes: modeReports,
    repositories: manifests,
    tasks: quality,
  };
  await writeFile(
    join(directory, "scores.jsonl"),
    observations.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
  await writeJson(join(directory, "report.json"), report);
  await writeFile(join(directory, "report.md"), markdownReport(report));
  return report;
}

export async function main() {
  const report = await aggregate(process.argv[2]);
  if (!report.integrity_passed) process.exitCode = 1;
}
