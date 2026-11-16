import assert from "node:assert/strict";
import { join } from "node:path";
import { writeJson } from "../core/io.mjs";
import { GOLD_STAGES, summarizeGoldStages } from "../metrics/gold-stages.mjs";

const REPORT_ROWS = GOLD_STAGES.filter(({ label }) => label).map(
  ({ id, label }) => [id, label],
);
const MODES = ["fts", "vector", "hybrid"];
const LEVELS = ["file", "entity"];
const QUALIFICATIONS = ["formal", "diagnostic"];

const countWithin = (count, maximum = Number.MAX_SAFE_INTEGER) =>
  Number.isSafeInteger(count) && count >= 0 && count <= maximum;

// Only fixed labels and numeric counts from the artifact reach PR comments.
export function validateGoldStageSummary(summary) {
  assert.equal(summary?.version, 1, "Unsupported Gold stage summary");
  const { calls, observed_calls, invalid_calls, groups } = summary;
  assert.ok(countWithin(calls), "Invalid Gold stage call count");
  assert.ok(
    countWithin(observed_calls, calls),
    "Invalid observed Gold stage call count",
  );
  assert.ok(
    countWithin(invalid_calls, calls - observed_calls),
    "Invalid failed Gold stage call count",
  );
  assert.ok(
    Array.isArray(groups) && groups.length <= 12,
    "Invalid Gold stage groups",
  );
  const seen = new Set();
  const validated = groups.map(
    ({ qualification, level, mode, targets, stages }) => {
      assert.ok(
        QUALIFICATIONS.includes(qualification) &&
          LEVELS.includes(level) &&
          MODES.includes(mode),
        "Invalid Gold stage group",
      );
      const group = `${qualification}/${level}/${mode}`;
      assert.ok(!seen.has(group), "Duplicate Gold stage group");
      seen.add(group);
      assert.ok(countWithin(targets), "Invalid Gold stage target count");
      const coverage = {};
      for (const [key] of REPORT_ROWS) {
        if (
          (key === "fts" && mode === "vector") ||
          (key === "vector" && mode === "fts")
        )
          continue;
        const { covered, unknown } = stages?.[key] ?? {};
        assert.ok(
          countWithin(covered, targets) &&
            countWithin(unknown, targets - covered),
          "Invalid Gold stage coverage",
        );
        coverage[key] = { covered, unknown };
      }
      return { qualification, level, mode, targets, stages: coverage };
    },
  );
  return {
    version: 1,
    calls,
    observed_calls,
    invalid_calls,
    groups: validated,
  };
}

export async function writeGoldStages(directory, calls) {
  const summary = summarizeGoldStages(calls);
  await writeJson(join(directory, "gold-stages.json"), { summary, calls });
  return summary;
}

function stageCell(group, key) {
  const entry = group?.stages[key];
  if (!entry) return "—";
  const { covered, unknown } = entry;
  if (unknown === group.targets) return "N/A";
  const value = `${covered}/${group.targets}`;
  return unknown ? `${value} (${unknown} unknown)` : value;
}

export function goldStagesMarkdown(summary, { heading = true } = {}) {
  if (!summary) return [];
  const lines = [
    "",
    ...(heading ? ["### Gold coverage by retrieval stage", ""] : []),
    `Verified captures: **${summary.observed_calls}/${summary.calls}** planned calls. Invalid captures: **${summary.invalid_calls}**.`,
    "",
    "Each row shows Gold coverage across all five calls. The decay rows use the same loaded candidates, RRF scores, static scores, and 10-result limit. Decay on uses the actual results. N/A means unknown, and — means the route did not run.",
    "",
    "See `gold-stages.json` for all stages and per-target ranks.",
    "Diagnostic MCP latency includes stage capture, serialization and transfer; compare normal search latency with capture disabled on both versions.",
    "",
  ];
  for (const qualification of QUALIFICATIONS) {
    for (const level of LEVELS) {
      const groups = summary.groups.filter(
        (group) =>
          group.qualification === qualification && group.level === level,
      );
      if (!groups.length) continue;
      lines.push(
        `#### ${level === "file" ? "File" : "Entity"} Gold (${qualification})`,
        "",
        "| Stage | FTS | Vector | Hybrid |",
        "| --- | ---: | ---: | ---: |",
      );
      for (const [key, name] of REPORT_ROWS) {
        const cells = MODES.map((mode) =>
          stageCell(
            groups.find((item) => item.mode === mode),
            key,
          ),
        );
        lines.push(`| ${name} | ${cells.join(" | ")} |`);
      }
      lines.push("");
    }
  }
  return lines;
}
