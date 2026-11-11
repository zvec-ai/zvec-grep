import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { relativePath } from "../core/response.mjs";
import { ndcgAtK } from "./ndcg.mjs";

const CUTOFFS = [1, 5, 10];
export const QA_METRIC_FIELDS = [
  ...CUTOFFS.map((k) => `hit_at_${k}`),
  ...CUTOFFS.map((k) => `recall_at_${k}`),
  "rr_at_10",
  "ndcg_at_10",
];
const hash = (value) => createHash("sha256").update(value).digest("hex");
const lines = (text) => text.split(/\r?\n/);
const canonicalSymbol = (name) => name.replaceAll("::", ".");

/** These are source annotation IDs, never IDs produced by the candidate parser. */
export function validateQaPair(task, pair) {
  assert.equal(pair.query, task.query, `${task.task_id}: QA query changed`);
  assert.equal(pair.query_sha256, task.query_sha256);
  assert.equal(pair.repository, task.repository);
  assert.equal(pair.repository_commit, task.repository_commit);
  assert.ok(["reviewed", "disputed"].includes(pair.review?.correctness));
  assert.ok(["reviewed", "not_reviewed"].includes(pair.review?.completeness));
  assert.equal(typeof pair.scoring_eligible, "boolean");
  assert.equal(typeof pair.diagnostic_eligible, "boolean");
  if (pair.scoring_eligible) {
    assert.equal(pair.review.correctness, "reviewed");
    assert.equal(pair.review.completeness, "reviewed");
  }
  if (pair.diagnostic_eligible)
    assert.equal(pair.review.correctness, "reviewed");
  const ids = new Set();
  const declarations = new Set();
  for (const level of ["file", "entity"]) {
    assert.ok(Array.isArray(pair[`${level}_targets`]));
    assert.ok(
      pair[`${level}_targets`].length > 0,
      `${level}: missing QA targets`,
    );
    const paths = new Set();
    for (const target of pair[`${level}_targets`]) {
      assert.ok(target.id && !ids.has(target.id), "duplicate QA target ID");
      ids.add(target.id);
      assert.ok(
        relativePath(target.path),
        "QA targets require complete relative paths",
      );
      assert.match(target.file_sha256, /^[a-f0-9]{64}$/);
      if (level === "file") {
        assert.ok(!paths.has(target.path), "duplicate QA file target");
        paths.add(target.path);
        continue;
      }
      assert.ok(
        pair.file_targets.some(
          (file) =>
            file.id === target.file_target_id && file.path === target.path,
        ),
      );
      assert.equal(typeof target.symbol, "string");
      assert.ok(
        Number.isSafeInteger(target.start_line) && target.start_line > 0,
      );
      assert.ok(
        Number.isSafeInteger(target.end_line) &&
          target.end_line >= target.start_line,
      );
      assert.ok(
        Number.isSafeInteger(target.declaration_start_line) &&
          target.declaration_start_line >= target.start_line,
      );
      assert.ok(
        Number.isSafeInteger(target.declaration_end_line) &&
          target.declaration_end_line >= target.declaration_start_line &&
          target.declaration_end_line <= target.end_line,
      );
      // Annotation IDs and parser boundary changes cannot create another vote
      // for the same declaration. Distinct declarations/overloads retain their
      // own source positions even when they share a qualified name.
      const identity = JSON.stringify([
        target.path,
        canonicalSymbol(target.symbol),
        target.declaration_start_line,
      ]);
      assert.ok(!declarations.has(identity), "duplicate QA entity declaration");
      declarations.add(identity);
      assert.equal(
        hash(target.definition_text),
        target.definition_sha256,
        "QA definition content changed",
      );
      const source = lines(target.definition_text);
      if (source.at(-1) === "") source.pop();
      assert.equal(source.length, target.end_line - target.start_line + 1);
      const declaration = source
        .slice(
          target.declaration_start_line - target.start_line,
          target.declaration_end_line - target.start_line + 1,
        )
        .join("\n");
      assert.equal(
        declaration,
        target.declaration_text.replace(/\r\n/g, "\n").replace(/\n$/, ""),
        "QA declaration must belong to the fixed definition",
      );
      for (const anchor of target.anchors) {
        assert.equal(
          hash(anchor.text),
          anchor.sha256,
          "QA anchor content changed",
        );
        assert.ok(
          anchor.start_line >= target.start_line &&
            anchor.end_line <= target.end_line,
        );
        assert.equal(
          source
            .slice(
              anchor.start_line - target.start_line,
              anchor.end_line - target.start_line + 1,
            )
            .join("\n"),
          anchor.text.replace(/\r\n/g, "\n").replace(/\n$/, ""),
        );
      }
    }
  }
}

function validateItems(items) {
  assert.ok(Array.isArray(items), "QA scoring requires public items");
  for (const [index, item] of items.entries()) {
    assert.equal(
      item.rank,
      index + 1,
      "QA scoring requires original consecutive ranks",
    );
    assert.ok(
      relativePath(item.path),
      "QA scoring requires repository-relative public paths",
    );
    if (item.range?.kind === "text") {
      assert.ok(
        Number.isSafeInteger(item.range.start_line) &&
          item.range.start_line > 0 &&
          Number.isSafeInteger(item.range.end_line) &&
          item.range.end_line >= item.range.start_line,
        "invalid public source range",
      );
    }
  }
}

function entityMatch(item, targets, entityByteRanges) {
  const sameFile = targets.filter((target) => target.path === item.path);
  if (item.range?.kind !== "text")
    return { hits: [], possible: sameFile.map((target) => target.id) };
  const { start_line: start, end_line: end } = item.range;
  const hits = [],
    possible = [];
  for (const target of sameFile) {
    if (start > target.start_line || end < target.end_line) continue;
    if (entityByteRanges) {
      const { start_byte: startByte, end_byte: endByte } =
        item.source_bytes ?? {};
      if (startByte == null || endByte == null) {
        possible.push(target.id);
      } else {
        assert.ok(
          Number.isSafeInteger(startByte) &&
            Number.isSafeInteger(endByte) &&
            startByte >= 0 &&
            endByte > startByte,
          "invalid Entity source bytes",
        );
        if (startByte <= target.start_byte && endByte >= target.end_byte)
          hits.push(target.id);
      }
      continue;
    }
    // Public ranges identify the Entity in the pinned source, not its preview.
    // A single-line target on a boundary needs columns that this output omits.
    if (
      target.start_line === target.end_line &&
      (start === target.start_line || end === target.end_line)
    )
      possible.push(target.id);
    else hits.push(target.id);
  }
  return { hits, possible };
}

function entityNdcg(relevantRanks, targets, k) {
  const gains = new Map();
  for (const rank of relevantRanks)
    if (rank <= k) gains.set(rank, (gains.get(rank) ?? 0) + 1);
  const dcg = [...gains].reduce(
    (sum, [rank, gain]) => sum + gain / Math.log2(rank + 1),
    0,
  );
  // An Entity cannot cross files. The ideal result for each file contains all
  // its Gold targets; this fixed upper bound does not depend on parser splits.
  const perFile = new Map();
  for (const target of targets)
    perFile.set(target.path, (perFile.get(target.path) ?? 0) + 1);
  const ideal = [...perFile.values()]
    .sort((a, b) => b - a)
    .slice(0, k)
    .reduce((sum, gain, index) => sum + gain / Math.log2(index + 2), 0);
  return ideal ? dcg / ideal : 0;
}

function rankMetrics(items, targets, match, ndcg = ndcgAtK) {
  const found = new Map();
  const unknown = [];
  for (const item of items) {
    if (item.rank > 10) break;
    const result = match(item, targets);
    for (const hit of result.hits)
      if (!found.has(hit)) found.set(hit, item.rank);
    if (result.possible.length)
      unknown.push({ rank: item.rank, target_ids: result.possible });
  }
  const result = {
    n_relevant: targets.length,
    target_ranks: targets.map((target) => found.get(target.id) ?? null),
    unknown_ranks: unknown,
  };
  for (const k of CUTOFFS) {
    const ranks = [...found.values()].filter((rank) => rank <= k);
    const uncertain = unknown.filter((item) => item.rank <= k);
    const newTarget = uncertain.some((item) =>
      item.target_ids.some((id) => (found.get(id) ?? Infinity) > k),
    );
    result[`hit_at_${k}`] = ranks.length ? 1 : uncertain.length ? null : 0;
    result[`recall_at_${k}`] = newTarget ? null : ranks.length / targets.length;
  }
  const first = Math.min(...found.values(), Infinity);
  const earlier = unknown.some((item) => item.rank < first);
  result.first_hit_rank = earlier
    ? "unknown"
    : first === Infinity
      ? "not_in_top10"
      : first;
  result.rr_at_10 = earlier ? null : first === Infinity ? 0 : 1 / first;
  const affectsRanking = unknown.some((item) =>
    item.target_ids.some((id) => item.rank < (found.get(id) ?? Infinity)),
  );
  result.ndcg_at_10 = affectsRanking
    ? null
    : ndcg([...found.values()], targets.length, 10);
  return result;
}

function eligibility(pair) {
  if (!pair)
    return {
      status: "unavailable",
      formal_eligible: false,
      reason: "qa_gold_missing",
    };
  if (
    pair.scoring_eligible === true &&
    pair.review?.correctness === "reviewed" &&
    pair.review?.completeness === "reviewed"
  )
    return { status: "scored", formal_eligible: true, reason: null };
  if (
    pair.diagnostic_eligible === true &&
    pair.review?.correctness === "reviewed"
  )
    return {
      status: "diagnostic",
      formal_eligible: false,
      reason: "gold_completeness_not_reviewed",
    };
  return {
    status: "unavailable",
    formal_eligible: false,
    reason: "gold_review_unresolved",
  };
}

/** Score only public evidence. Diagnostic labels never authorize formal totals. */
export function scoreQaRetrieval(
  items,
  pair,
  {
    executionStatus = "success",
    status = "scored",
    entityByteRanges = false,
  } = {},
) {
  const review = eligibility(pair);
  if (review.status === "unavailable")
    return { ...review, file: null, entity: null };
  if (executionStatus !== "success" || status === "harness_invalid")
    return {
      status: "unavailable",
      formal_eligible: review.formal_eligible,
      reason:
        status === "harness_invalid"
          ? "harness_invalid"
          : `execution_${executionStatus}`,
      file: null,
      entity: null,
    };
  validateItems(items);
  assert.ok(
    pair.file_targets.length && pair.entity_targets.length,
    "QA scoring needs nonempty labels",
  );
  return {
    ...review,
    file: rankMetrics(items, pair.file_targets, (item, targets) => {
      const target = targets.find((target) => target.path === item.path);
      return { hits: target ? [target.id] : [], possible: [] };
    }),
    entity: rankMetrics(
      items,
      pair.entity_targets,
      (item, targets) => entityMatch(item, targets, entityByteRanges),
      (ranks, _, k) => entityNdcg(ranks, pair.entity_targets, k),
    ),
  };
}

/** Retain only public evidence required for independent replay of Entity scores. */
export function publicQaItems(items) {
  return items.map(
    ({ rank, path, range, matched_by, metadata = [], source_lines = [] }) => ({
      rank,
      path,
      range,
      matched_by,
      metadata,
      source_lines,
    }),
  );
}

/** A metric receives one question vote only when all five observations are known. */
export function summarizeRepeatedQaRetrieval(calls, pair, options) {
  assert.equal(calls.length, 5, "QA quality requires five calls");
  assert.deepEqual(
    calls.map((call) => call.repetition),
    [1, 2, 3, 4, 5],
  );
  const review = eligibility(pair);
  if (review.status === "unavailable")
    return { ...review, file: null, entity: null };
  const scores = calls.map((call) =>
    scoreQaRetrieval(call.items, pair, {
      ...options,
      executionStatus: call.execution_status,
      status: call.status,
    }),
  );
  const result = { ...review };
  for (const level of ["file", "entity"]) {
    const scoredCalls = {};
    result[level] = { n_relevant: pair[`${level}_targets`].length };
    for (const field of QA_METRIC_FIELDS) {
      const values = scores
        .map((score) => score[level]?.[field])
        .filter((value) => Number.isFinite(value));
      scoredCalls[field] = values.length;
      result[level][field] =
        values.length === 5 ? values.reduce((a, b) => a + b, 0) / 5 : null;
    }
    result[level].scored_calls = scoredCalls;
  }
  return result;
}

/** Keep formal and diagnostic results separate, with a denominator per metric. */
export function summarizeQaRetrieval(rows) {
  const ordered = [...rows].sort((a, b) =>
    String(a.task_id).localeCompare(String(b.task_id), "en"),
  );
  const selected = ordered.map((row) => ({
    task_id: row.task_id,
    result:
      row.status === "harness_invalid"
        ? null
        : (row.qa_quality_mean ?? row.qa_retrieval),
  }));
  function group(status) {
    const eligible = selected.filter((row) => row.result?.status === status);
    const summary = { eligible_tasks: eligible.length };
    for (const level of ["file", "entity"]) {
      const scores = eligible.map((row) => row.result[level]);
      const metricCounts = {};
      summary[level] = {
        scored_tasks: scores.filter((score) =>
          QA_METRIC_FIELDS.some((field) => Number.isFinite(score?.[field])),
        ).length,
      };
      for (const field of QA_METRIC_FIELDS) {
        const key = field === "rr_at_10" ? "mrr_at_10" : field;
        const values = scores
          .map((score) => score?.[field])
          .filter((value) => Number.isFinite(value));
        summary[level][key] = values.length
          ? values.reduce((a, b) => a + b, 0) / values.length
          : null;
        metricCounts[key] = values.length;
      }
      summary[level].metric_task_counts = metricCounts;
    }
    return summary;
  }
  return {
    planned_tasks: rows.length,
    formal: group("scored"),
    diagnostic: group("diagnostic"),
    unavailable_tasks: selected
      .filter((row) => !row.result || row.result.status === "unavailable")
      .map((row) => ({
        task_id: row.task_id,
        reason: row.result?.reason ?? "invalid_evidence",
      })),
  };
}
