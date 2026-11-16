import assert from "node:assert/strict";
import { goldPaths } from "../core/gold-stage-capture.mjs";
import { entityMatch } from "./entities.mjs";

const LIMIT = 10;

const GOLD_STAGES = [
  ["index", null],
  ["fts", "index"],
  ["vector", "index"],
  ["recall", "index"],
  ["merged", "recall"],
  ["fused", "merged"],
  ["loaded", "fused"],
  ["selected", "loaded"],
  ["static_top10", "loaded"],
  ["actual_top10", "loaded"],
];

function location(path, range, sourceOffset) {
  return {
    path,
    range: range?.text ? {
      kind: "text",
      start_line: range.text.start.line,
      end_line: range.text.end.line,
    } : null,
    source_bytes: range?.text ? {
      start_byte: range.text.start.byte_offset + sourceOffset,
      end_byte: range.text.end.byte_offset + sourceOffset,
    } : null,
  };
}

function bestRank(matches, getRank, limit = Infinity) {
  let best = Infinity;
  let possible = Infinity;
  for (const { entry, match } of matches) {
    const rank = getRank(entry);
    if (!Number.isSafeInteger(rank) || rank <= 0 || rank > limit || match === false) continue;
    if (match === null) possible = Math.min(rank, possible);
    else best = Math.min(rank, best);
  }
  return {
    covered: best < Infinity ? true : possible < Infinity ? null : false,
    rank: possible < best ? "unobserved" : best < Infinity ? best : "miss",
  };
}

function matchTarget(path, range, target, level, useBytes, sourceOffset) {
  if (level === "file") return path === target.path;
  const { hits, possible } = entityMatch(location(path, range, sourceOffset), [target], useBytes);
  return hits.length > 0 ? true : possible.length > 0 ? null : false;
}

function applicable(mode) {
  return GOLD_STAGES.filter(([key]) =>
    (key !== "fts" && key !== "vector") || mode === "hybrid" || mode === key,
  );
}

function validateCapture(capture, paths, mode, items, useBytes, sourceOffsets) {
  assert.equal(capture.version, 1, "unknown stage capture version");
  assert.equal(capture.limit, LIMIT, "unexpected retrieval limit");
  const routes = mode === "hybrid" ? ["fts", "vector"] : [mode];
  assert.deepEqual(Object.keys(capture.routes).sort(), routes, "unexpected recall routes");
  assert.ok(Object.keys(capture.files).every((path) => paths.has(path)), "unexpected watched file");
  const { merged, fused, loaded, selected } = capture.sizes;
  assert.ok([merged, fused, loaded, selected].every((size) => Number.isSafeInteger(size) && size >= 0));
  assert.ok(merged >= fused && fused >= loaded && selected === Math.min(LIMIT, loaded), "invalid pool sizes");
  assert.equal(selected, items.length, "engine and public result counts differ");
  if (!capture.index_observed) return;
  const seen = new Set();
  for (const [path, entries] of Object.entries(capture.files)) {
    for (const entry of Object.values(entries)) {
      if (entry.final_rank === null) continue;
      const item = items[entry.final_rank - 1];
      const observed = location(path, entry.loaded_range, sourceOffsets[path] ?? 0);
      assert.ok(item && item.rank === entry.final_rank && !seen.has(item.rank), "invalid selected rank");
      assert.equal(item.path, path, "engine and public result paths differ");
      if (item.range?.kind === "text" && observed.range) {
        assert.equal(item.range.start_line, observed.range.start_line, "engine and public start lines differ");
        assert.equal(item.range.end_line, observed.range.end_line, "engine and public end lines differ");
      }
      if (useBytes && item.source_bytes) {
        assert.deepEqual(item.source_bytes, observed.source_bytes, "engine and public byte ranges differ");
      }
      seen.add(item.rank);
    }
  }
  assert.ok(items.every((item) => !paths.has(item.path) || seen.has(item.rank)), "watched public result missing from capture");
}

function scoreTarget(capture, target, level, useBytes, sourceOffset) {
  const entries = Object.values(capture.files[target.path] ?? {});
  const matches = (field) => entries.map((entry) => ({
    entry,
    match: matchTarget(target.path, entry[field], target, level, useBytes, sourceOffset),
  }));
  const canonical = matches("range");
  const loaded = matches("loaded_range");
  const at = (field) => (entry) => entry[field];
  const fts = (entry) => entry.recall.fts;
  const vector = (entry) => entry.recall.vector;
  return {
    index: bestRank(canonical, (entry) => entry.searchable ? 1 : null),
    fts: bestRank(canonical, fts),
    vector: bestRank(canonical, vector),
    recall: bestRank(canonical, (entry) => Math.min(fts(entry) ?? Infinity, vector(entry) ?? Infinity)),
    merged: bestRank(canonical, (entry) => entry.merged ? 1 : null),
    fused: bestRank(canonical, at("rrf_rank")),
    loaded: bestRank(loaded, at("loaded_rrf_rank")),
    selected: bestRank(loaded, at("final_rank")),
    static_top10: bestRank(loaded, at("static_rank"), LIMIT),
    actual_top10: bestRank(loaded, at("final_rank"), LIMIT),
    static: bestRank(loaded, at("static_rank")),
  };
}

export function scoreGoldStages({ capture, pair, items, taskId, mode, repetition, qualification, useBytes = false, sourceOffsets = {} }) {
  let state = capture ? (capture.index_observed ? "observed" : "index_unobserved") : "unobserved";
  let error;
  if (capture) {
    try {
      validateCapture(capture, new Set(goldPaths(pair)), mode, items, useBytes, sourceOffsets);
    } catch (failure) {
      state = "invalid";
      error = failure.message;
    }
  }
  const targets = ["file", "entity"].flatMap((level) => pair[`${level}_targets`].map((target) => {
    const stages = state === "observed"
      ? scoreTarget(capture, target, level, useBytes, sourceOffsets[target.path] ?? 0)
      : {};
    const coverage = Object.fromEntries(applicable(mode).map(([key]) => [key, stages[key]?.covered ?? null]));
    const ranks = Object.fromEntries([
      ...applicable(mode).filter(([key]) => ["fts", "vector", "fused", "loaded", "selected"].includes(key)).map(([key]) => key),
      "static",
    ].map((key) => [key, stages[key]?.rank ?? "unobserved"]));
    return { level, id: target.id, path: target.path, symbol: target.symbol, coverage, ranks };
  }));
  return {
    task_id: taskId, mode, repetition, qualification, state, error,
    index_read_ms: capture?.index_read_ms ?? null,
    sizes: capture?.sizes ?? null,
    routes: capture?.routes ?? null,
    targets,
  };
}

export function summarizeGoldStages(calls) {
  const groups = [];
  for (const qualification of ["formal", "diagnostic"]) {
    for (const mode of ["fts", "vector", "hybrid"]) {
      const selected = calls.filter((row) => row.mode === mode && row.qualification === qualification);
      if (!selected.length) continue;
      for (const level of ["file", "entity"]) {
        const targets = selected.flatMap((row) => row.targets.filter((target) => target.level === level));
        const stages = Object.fromEntries(applicable(mode).map(([key, baseline]) => {
          const covered = targets.filter(({ coverage }) => coverage[key] === true).length;
          const unknown = targets.filter(({ coverage }) => coverage[key] === null).length;
          const lost = baseline === null ? null : targets.filter(({ coverage }) =>
            coverage[baseline] === true && coverage[key] === false).length;
          const lossUnknown = baseline === null ? null : targets.filter(({ coverage }) =>
            (coverage[baseline] === null && coverage[key] !== true) ||
            (coverage[baseline] === true && coverage[key] === null)).length;
          return [key, { covered, unknown, lost, loss_unknown: lossUnknown }];
        }));
        groups.push({ qualification, mode, level, calls: selected.length, targets: targets.length, stages });
      }
    }
  }
  const captured = calls.filter(({ state }) => state === "observed");
  const indexRead = captured.map(({ index_read_ms }) => index_read_ms);
  return {
    version: 1,
    calls: calls.length,
    observed_calls: captured.length,
    invalid_calls: calls.filter(({ state }) => state === "invalid").length,
    mean_index_read_ms: indexRead.length ? indexRead.reduce((sum, ms) => sum + ms, 0) / indexRead.length : null,
    groups,
  };
}
