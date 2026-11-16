import { join } from "node:path";
import { writeJson } from "../core/io.mjs";
import { summarizeGoldStages } from "../metrics/gold-stages.mjs";

const REPORT_ROWS = [
  ["index", "Gold covered by index", "coverage"],
  ["fts", "Gold recalled by FTS", "coverage"],
  ["vector", "Gold recalled by Vector", "coverage"],
  ["loaded", "Gold lost during Entity loading", "loss"],
  ["selected", "Gold lost at final top 10", "loss"],
];

export async function writeGoldStages(directory, calls) {
  const summary = summarizeGoldStages(calls);
  await writeJson(join(directory, "gold-stages.json"), { summary, calls });
  return summary;
}

function stageCell(group, key, metric) {
  const entry = group?.stages[key];
  if (!entry) return "—";
  const isLoss = metric === "loss";
  const unknown = isLoss ? entry.loss_unknown : entry.unknown;
  if (unknown === group.targets) return "N/A";
  const value = isLoss ? entry.lost : `${entry.covered}/${group.targets}`;
  return unknown ? `${value} (${unknown} unknown)` : String(value);
}

export function goldStagesMarkdown(summary) {
  if (!summary) return [];
  const lines = [
    "",
    "### Gold coverage by retrieval stage",
    "",
    `Verified captures: **${summary.observed_calls}/${summary.calls}** planned calls. Invalid captures: **${summary.invalid_calls}**.`,
    "",
    "Coverage sums Gold targets across all five calls. Loading losses start from the fused pool. Final losses start from the loaded pool. N/A means unknown, and — means the route did not run.",
    "",
    "See `gold-stages.json` for all stages, diagnostic cutoffs, and per-target ranks.",
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
      for (const [key, name, metric] of REPORT_ROWS) {
        const cells = ["fts", "vector", "hybrid"].map((mode) =>
          stageCell(groups.find((item) => item.mode === mode), key, metric),
        );
        lines.push(`| ${name} | ${cells.join(" | ")} |`);
      }
      lines.push("");
    }
  }
  return lines;
}
