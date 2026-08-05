import type { ZvecGrepContextResult } from "../../index.js";

export function printDebug(
  result: ZvecGrepContextResult,
  options: { trace?: boolean } = {},
): void {
  console.error(
    `source=${result.source} coverage=${result.coverage} items=${result.items.length}`,
  );

  if (result.collection) {
    console.error(
      `collection=${result.collection.name} path=${result.collection.path}`,
    );
  }

  if (result.diagnostics.index) {
    const routes = result.diagnostics.index.routes
      .map((route) => `${route.mode}:${route.query}`)
      .join(", ");
    console.error(
      `index_hits=${result.diagnostics.index.hitsReturned} routes=${routes}`,
    );
  }

  if (result.diagnostics.rg) {
    const rg = result.diagnostics.rg;
    console.error(
      `rg=${rg.backend} limit=${rg.limit ?? "all"} truncated=${rg.truncated}`,
    );
    console.error(
      `rg_command=${shellArg(rg.command)} ${rg.args.map(shellArg).join(" ")}`,
    );
    console.error(`rg_ignored=${rg.ignoredDirectories.join(",")}`);
    if (rg.missingPaths && rg.missingPaths.length > 0) {
      console.error(`rg_missing=${rg.missingPaths.join(",")}`);
    }
  }

  if (result.diagnostics.structure) {
    const structure = result.diagnostics.structure;
    const status = structure.truncated ? "partial" : "full";
    console.error(
      `structural_enrichment=${status} source=${structure.source} matched_files=${structure.matchedFiles} ` +
        `parsed_files=${structure.parsedFiles} enriched_files=${structure.enrichedFiles} ` +
        `enriched_items=${structure.enrichedItems} skipped_files=${structure.skippedFiles} file_limit=${structure.fileLimit}`,
    );
  }

  if (result.diagnostics.timings && result.diagnostics.timings.length > 0) {
    console.error(
      `timings=${result.diagnostics.timings.map(formatTiming).join(" ")}`,
    );
  }

  if (options.trace) {
    printTrace(result);
  }
}

function printTrace(result: ZvecGrepContextResult): void {
  if (result.source !== "index") {
    console.error(`trace=unavailable source=${result.source}`);
    return;
  }

  const traced = result.items.filter((item) => item.trace);
  if (traced.length === 0) {
    console.error("trace=unavailable reason=no-hit-trace");
    return;
  }

  console.error(`trace=inline items=${traced.length}`);
}

function shellArg(value: string): string {
  return /^[A-Za-z0-9_./:=@+-]+$/.test(value) ? value : JSON.stringify(value);
}

function formatTiming(
  entry: NonNullable<ZvecGrepContextResult["diagnostics"]["timings"]>[number],
): string {
  const count = entry.count && entry.count > 1 ? `(${entry.count}x)` : "";
  return `${entry.name}:${entry.durationMs}ms${count}`;
}
