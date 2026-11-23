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
const meanWithin = (count, maximum) =>
  Number.isFinite(count) && count >= 0 && count <= maximum + 1e-9;
const displayCount = (count) => Number(count.toFixed(6));

// Validate coverage before rendering stage reports.
export function validateGoldStageSummary(summary) {
  assert.equal(
    summary?.version,
    2,
    "Unsupported Gold stage summary; rebuild from per-call evidence",
  );
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
          meanWithin(covered, targets) &&
            meanWithin(unknown, targets - covered),
          "Invalid Gold stage coverage",
        );
        coverage[key] = { covered, unknown };
      }
      return { qualification, level, mode, targets, stages: coverage };
    },
  );
  return {
    version: 2,
    calls,
    observed_calls,
    invalid_calls,
    groups: validated,
  };
}

export async function writeGoldStages(
  directory,
  calls,
  filename = "gold-stages.json",
) {
  const summary = summarizeGoldStages(calls);
  await writeJson(join(directory, filename), { summary, calls });
  return summary;
}

function stageCell(group, key) {
  const entry = group?.stages[key];
  if (!entry) return "—";
  const { covered, unknown } = entry;
  if (unknown === group.targets) return "N/A";
  const value = `${displayCount(covered)}/${group.targets}`;
  return unknown ? `${value} (${displayCount(unknown)} unknown)` : value;
}

export function goldStageCaptureWarning(summary) {
  if (!summary || summary.observed_calls === summary.calls) return null;
  const { calls, observed_calls, invalid_calls } = summary;
  const unobserved = calls - observed_calls - invalid_calls;
  return `⚠️ Stage capture incomplete: **${observed_calls}/${calls}** verified, **${invalid_calls}** invalid, **${unobserved}** unobserved. Unverified stage coverage is unknown. Retrieval quality results are reported separately.`;
}

export function goldStagesMarkdown(summary) {
  if (!summary) return [];
  validateGoldStageSummary(summary);
  const captureWarning = goldStageCaptureWarning(summary);
  const lines = [
    "",
    "### Gold coverage by retrieval stage",
    "",
    ...(captureWarning ? [captureWarning, ""] : []),
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
        const cells = MODES.map((mode) => {
          const group = groups.find((item) => item.mode === mode);
          return stageCell(group, key);
        });
        lines.push(`| ${name} | ${cells.join(" | ")} |`);
      }
      lines.push("");
    }
  }
  return lines;
}
