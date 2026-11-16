import { join } from "node:path";
import { writeJson } from "../core/io.mjs";
import { GOLD_STAGES, summarizeGoldStages } from "../metrics/gold-stages.mjs";

export async function writeGoldStages(directory, calls) {
  const summary = summarizeGoldStages(calls);
  await writeJson(join(directory, "gold-stages.json"), { summary, calls });
  return summary;
}

export function goldStagesMarkdown(summary) {
  if (!summary) return [];
  const lines = [
    "",
    "### Gold coverage by retrieval stage",
    "",
    `Verified captures: **${summary.observed_calls}/${summary.calls}** planned calls. Invalid captures: **${summary.invalid_calls}**. See \`gold-stages.json\` for per-call Gold IDs, ranks, pool sizes and recall attempts.`,
    "",
    "Coverage counts Gold targets across all five calls. The denominator includes unobserved targets. A loss counts a target covered by the named reference stage that is missing at the current stage. Counts in parentheses are unobserved coverage or unverified losses. ‘N/A’ means all evidence for that cell is unknown. Formal and diagnostic labels are reported separately.",
    "",
    "The three @10 rows use the same loaded Entity pool. RRF order keeps the relative fusion order of loaded Entities. RRF and static-score cutoffs are diagnostic comparisons. Final selection uses the actual same-file decay and request limit.",
    "",
  ];
  if (summary.mean_index_read_ms !== null)
    lines.push(`Mean watched-file index read: **${summary.mean_index_read_ms.toFixed(2)} ms/call**. MCP latency also includes capture bookkeeping, sorting and writing.`, "");
  const label = new Map(GOLD_STAGES.map(([key, name]) => [key, name]));
  for (const qualification of ["formal", "diagnostic"]) {
    for (const level of ["file", "entity"]) {
      const groups = summary.groups.filter((group) => group.qualification === qualification && group.level === level);
      if (!groups.length) continue;
      lines.push(
        `#### ${level === "file" ? "File" : "Entity"} Gold (${qualification})`,
        "",
        "| Stage | Loss reference | FTS covered / Gold; lost | Vector covered / Gold; lost | Hybrid covered / Gold; lost |",
        "| --- | --- | --- | --- | --- |",
      );
      for (const [key, name, baseline] of GOLD_STAGES) {
        const cells = ["fts", "vector", "hybrid"].map((mode) => {
          const group = groups.find((item) => item.mode === mode);
          const entry = group?.stages[key];
          if (!entry) return "—";
          const { covered, unknown, lost, loss_unknown: lossUnknown } = entry;
          if (unknown === group.targets) return `N/A (${unknown}/${group.targets})`;
          const coverage = `${covered}/${group.targets}${unknown ? ` (${unknown})` : ""}`;
          return lost === null ? coverage : `${coverage}; ${lost}${lossUnknown ? ` (${lossUnknown})` : ""}`;
        });
        lines.push(`| ${name} | ${label.get(baseline) ?? "—"} | ${cells.join(" | ")} |`);
      }
      lines.push("");
    }
  }
  return lines;
}
