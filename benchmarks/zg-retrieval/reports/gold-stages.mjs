import { join } from "node:path";
import { writeJson } from "../core/io.mjs";
import { summarizeGoldStages } from "../metrics/gold-stages.mjs";

const REPORT_ROWS = [
  ["index", "Gold covered by index"],
  ["fts", "Gold recalled by FTS"],
  ["vector", "Gold recalled by Vector"],
  ["static_top10", "Top 10 coverage (same-file decay off)"],
  ["actual_top10", "Top 10 coverage (same-file decay on, actual)"],
];

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

export function goldStagesMarkdown(summary) {
  if (!summary) return [];
  const lines = [
    "",
    "### Gold coverage by retrieval stage",
    "",
    `Verified captures: **${summary.observed_calls}/${summary.calls}** planned calls. Invalid captures: **${summary.invalid_calls}**.`,
    "",
    "Each row shows the Gold targets covered at that stage, summed across all five calls. Both top-10 rows share the loaded pool and the same RRF and static scores. Decay off ranks by static score. Decay on uses the actual results. N/A means unknown, and — means the route did not run.",
    "",
    "See `gold-stages.json` for all stages and per-target ranks.",
    "",
  ];
  for (const qualification of ["formal", "diagnostic"]) {
    for (const level of ["file", "entity"]) {
      const groups = summary.groups.filter((group) => group.qualification === qualification && group.level === level);
      if (!groups.length) continue;
      lines.push(
        `#### ${level === "file" ? "File" : "Entity"} Gold (${qualification})`,
        "",
        "| Stage | FTS | Vector | Hybrid |",
        "| --- | ---: | ---: | ---: |",
      );
      for (const [key, name] of REPORT_ROWS) {
        const cells = ["fts", "vector", "hybrid"].map((mode) =>
          stageCell(groups.find((item) => item.mode === mode), key),
        );
        lines.push(`| ${name} | ${cells.join(" | ")} |`);
      }
      lines.push("");
    }
  }
  return lines;
}
