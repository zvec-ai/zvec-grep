import assert from "node:assert/strict";
import { goldPaths } from "../core/gold-stage-capture.mjs";
import { entityMatch, evidenceCompletionRank } from "./entities.mjs";

const LIMIT = 10;

// Scoring and display share stage relationships and labels. Capturing a new
// engine stage still needs an explicit mapping in scoreTarget below.
export const GOLD_STAGES = [
  { id: "index", parent: null, label: "Gold covered by index" },
  { id: "fts", parent: "index", ranked: true, label: "Gold recalled by FTS" },
  {
    id: "vector",
    parent: "index",
    ranked: true,
    label: "Gold recalled by Vector",
  },
  { id: "recall", parent: "index" },
  { id: "merged", parent: "recall" },
  { id: "fused", parent: "merged", ranked: true },
  { id: "loaded", parent: "fused", ranked: true },
  { id: "selected", parent: "loaded", ranked: true },
  {
    id: "rrf_top10",
    parent: "loaded",
    label: "Top 10 before rule ranking (RRF)",
  },
  {
    id: "actual_top10",
    parent: "loaded",
    label: "Top 10 after rule ranking (actual)",
  },
];

function location(path, range, sourceOffset) {
  const text = range?.text;
  return {
    path,
    range: text
      ? {
          kind: "text",
          start_line: text.start.line,
          // Rust ranges are half-open; public line spans include the last
          // covered line. Empty ranges keep their source position, even at EOF.
          end_line:
            text.end.line -
            Number(
              text.end.byte_column === 0 &&
                text.start.byte_offset < text.end.byte_offset,
            ),
        }
      : null,
    source_bytes: text
      ? {
          start_byte: text.start.byte_offset + sourceOffset,
          end_byte: text.end.byte_offset + sourceOffset,
        }
      : null,
  };
}

function spanRanks(matches, getRank, limit) {
  let best = Infinity;
  let possible = Infinity;
  for (const { entry, match } of matches) {
    const rank = getRank(entry);
    if (
      !Number.isSafeInteger(rank) ||
      rank <= 0 ||
      rank > limit ||
      match === false
    )
      continue;
    if (match === null) possible = Math.min(rank, possible);
    else best = Math.min(rank, best);
  }
  return { best, possible };
}

function bestRank(groups, getRank, limit = Infinity) {
  const ranks = groups.map((group) =>
    group.map((matches) => spanRanks(matches, getRank, limit)),
  );
  const best = evidenceCompletionRank(ranks, (rank) => rank.best);
  const possible = evidenceCompletionRank(ranks, (rank) =>
    Math.min(rank.best, rank.possible),
  );
  return {
    covered: best < Infinity ? true : possible < Infinity ? null : false,
    rank: possible < best ? "unobserved" : best < Infinity ? best : "miss",
  };
}

function matchTarget(path, range, target, level, useBytes, sourceOffset) {
  if (level === "file") return path === target.path;
  const { hits, possible } = entityMatch(
    location(path, range, sourceOffset),
    [target],
    useBytes,
  );
  return hits.length > 0 ? true : possible.length > 0 ? null : false;
}

function applicable(mode) {
  return GOLD_STAGES.filter(
    ({ id: key }) =>
      (key !== "fts" && key !== "vector") || mode === "hybrid" || mode === key,
  );
}

function validateCapture(capture, paths, mode, items, useBytes, sourceOffsets) {
  assert.equal(capture.version, 1, "unknown stage capture version");
  assert.equal(capture.limit, LIMIT, "unexpected retrieval limit");
  assert.equal(
    typeof capture.index_observed,
    "boolean",
    "invalid index observation flag",
  );
  const routes = mode === "hybrid" ? ["fts", "vector"] : [mode];
  assert.deepEqual(
    Object.keys(capture.routes).sort(),
    routes,
    "unexpected recall routes",
  );
  assert.ok(Array.isArray(capture.watched_paths), "missing observation scope");
  const watched = new Set(capture.watched_paths);
  assert.ok(
    [...paths].every((path) => watched.has(path)),
    "Gold path was not observed",
  );
  assert.ok(
    Object.keys(capture.files).every((path) => watched.has(path)),
    "unexpected watched file",
  );
  const { merged, fused, loaded, selected } = capture.sizes;
  assert.ok(
    [merged, fused, loaded, selected].every(
      (size) => Number.isSafeInteger(size) && size >= 0,
    ),
  );
  assert.ok(
    merged >= fused && fused >= loaded && selected === Math.min(LIMIT, loaded),
    "invalid pool sizes",
  );
  assert.equal(
    selected,
    items.length,
    "engine and public result counts differ",
  );
  if (!capture.index_observed) return;
  const seen = new Set();
  for (const [path, entries] of Object.entries(capture.files)) {
    // One daemon watches the union of its questions' paths. Byte-range
    // verification is available only for this question's frozen sources.
    if (!paths.has(path)) continue;
    for (const entry of Object.values(entries)) {
      if (entry.final_rank === null) continue;
      const item = items[entry.final_rank - 1];
      const observed = location(
        path,
        entry.loaded_range,
        sourceOffsets[path] ?? 0,
      );
      assert.ok(
        item && item.rank === entry.final_rank && !seen.has(item.rank),
        "invalid selected rank",
      );
      assert.equal(item.path, path, "engine and public result paths differ");
      let omitsTrailingNewline = false;
      if (item.range?.kind === "text" && observed.range) {
        assert.equal(
          item.range.start_line,
          observed.range.start_line,
          "engine and public start lines differ",
        );
        // Compact MCP headers use the last covered line. The canonical range
        // can end at column zero on the next line after an LF or CRLF.
        omitsTrailingNewline =
          observed.range.end_line < entry.loaded_range.text.end.line;
        assert.equal(
          item.range.end_line,
          observed.range.end_line,
          "engine and public end lines differ",
        );
      }
      if (useBytes && item.source_bytes) {
        assert.equal(
          item.source_bytes.start_byte,
          observed.source_bytes?.start_byte,
          "engine and public start bytes differ",
        );
        const endDifference =
          observed.source_bytes.end_byte - item.source_bytes.end_byte;
        assert.ok(
          omitsTrailingNewline
            ? endDifference === 1 || endDifference === 2
            : endDifference === 0,
          "engine and public end bytes differ",
        );
      }
      seen.add(item.rank);
    }
  }
  assert.ok(
    items.every((item) => !paths.has(item.path) || seen.has(item.rank)),
    "watched public result missing from capture",
  );
}

function scoreTarget(capture, target, level, useBytes, sourceOffset) {
  const entries = Object.values(capture.files[target.path] ?? {});
  const groups =
    level === "entity" && target.evidence_sets
      ? target.evidence_sets.map(({ spans }) =>
          spans.map((span) => ({
            ...span,
            path: target.path,
            id: target.id,
          })),
        )
      : [[target]];
  const matches = (field) =>
    groups.map((spans) =>
      spans.map((span) =>
        entries.map((entry) => ({
          entry,
          match: matchTarget(
            target.path,
            entry[field],
            span,
            level,
            useBytes,
            sourceOffset,
          ),
        })),
      ),
    );
  const canonical = matches("range");
  const loaded = matches("loaded_range");
  const at = (field) => (entry) => entry[field];
  const fts = (entry) => entry.recall.fts;
  const vector = (entry) => entry.recall.vector;
  return {
    index: bestRank(canonical, (entry) => (entry.searchable ? 1 : null)),
    fts: bestRank(canonical, fts),
    vector: bestRank(canonical, vector),
    recall: bestRank(canonical, (entry) =>
      Math.min(fts(entry) ?? Infinity, vector(entry) ?? Infinity),
    ),
    merged: bestRank(canonical, (entry) => (entry.merged ? 1 : null)),
    fused: bestRank(canonical, at("rrf_rank")),
    loaded: bestRank(loaded, at("loaded_rrf_rank")),
    selected: bestRank(loaded, at("final_rank")),
    rrf_top10: bestRank(loaded, at("loaded_rrf_rank"), LIMIT),
    actual_top10: bestRank(loaded, at("final_rank"), LIMIT),
    static: bestRank(loaded, at("static_rank")),
  };
}

export function scoreGoldStages({
  capture,
  pair,
  items,
  taskId,
  mode,
  repetition,
  qualification,
  useBytes = false,
  sourceOffsets = {},
}) {
  let state =
    capture != null
      ? capture.index_observed
        ? "observed"
        : "index_unobserved"
      : "unobserved";
  let error;
  const targetEntries = ["file", "entity"].flatMap((level) =>
    pair[`${level}_targets`].map((target) => ({ level, target })),
  );
  let targetStages = [];
  if (capture != null) {
    try {
      if (capture instanceof Error) throw capture;
      validateCapture(
        capture,
        new Set(goldPaths(pair)),
        mode,
        items,
        useBytes,
        sourceOffsets,
      );
      if (state === "observed")
        targetStages = targetEntries.map(({ level, target }) =>
          scoreTarget(
            capture,
            target,
            level,
            useBytes,
            sourceOffsets[target.path] ?? 0,
          ),
        );
    } catch (failure) {
      state = "invalid";
      error = failure.message;
    }
  }
  const targets = targetEntries.map(({ level, target }, index) => {
    const stages = targetStages[index] ?? {};
    const coverage = Object.fromEntries(
      applicable(mode).map(({ id }) => [id, stages[id]?.covered ?? null]),
    );
    const ranks = Object.fromEntries(
      [
        ...applicable(mode)
          .filter(({ ranked }) => ranked)
          .map(({ id }) => id),
        "static",
      ].map((key) => [key, stages[key]?.rank ?? "unobserved"]),
    );
    return {
      level,
      id: target.id,
      path: target.path,
      symbol: target.symbol,
      coverage,
      ranks,
    };
  });
  return {
    task_id: taskId,
    mode,
    repetition,
    qualification,
    state,
    error,
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
      const selected = calls.filter(
        (row) => row.mode === mode && row.qualification === qualification,
      );
      if (!selected.length) continue;
      for (const level of ["file", "entity"]) {
        const targets = selected.flatMap((row) =>
          row.targets.filter((target) => target.level === level),
        );
        const stages = Object.fromEntries(
          applicable(mode).map(({ id: key, parent: baseline }) => {
            const covered = targets.filter(
              ({ coverage }) => coverage[key] === true,
            ).length;
            const unknown = targets.filter(
              ({ coverage }) => coverage[key] === null,
            ).length;
            const lost =
              baseline === null
                ? null
                : targets.filter(
                    ({ coverage }) =>
                      coverage[baseline] === true && coverage[key] === false,
                  ).length;
            const lossUnknown =
              baseline === null
                ? null
                : targets.filter(
                    ({ coverage }) =>
                      (coverage[baseline] === null && coverage[key] !== true) ||
                      (coverage[baseline] === true && coverage[key] === null),
                  ).length;
            return [key, { covered, unknown, lost, loss_unknown: lossUnknown }];
          }),
        );
        groups.push({
          qualification,
          mode,
          level,
          calls: selected.length,
          targets: targets.length,
          stages,
        });
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
    mean_index_read_ms: indexRead.length
      ? indexRead.reduce((sum, ms) => sum + ms, 0) / indexRead.length
      : null,
    groups,
  };
}
