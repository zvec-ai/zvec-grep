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
      `collection=${result.collection.name} anonymous=${result.collection.anonymous} path=${result.collection.path}`,
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

  if (result.diagnostics.fallback) {
    const fallback = result.diagnostics.fallback;
    console.error(
      `fallback=${fallback.backend} limit=${fallback.limit ?? "all"} truncated=${fallback.truncated}`,
    );
    console.error(
      `fallback_command=${shellArg(fallback.command)} ${fallback.args.map(shellArg).join(" ")}`,
    );
    console.error(`fallback_ignored=${fallback.ignoredDirectories.join(",")}`);
    if (fallback.missingPaths && fallback.missingPaths.length > 0) {
      console.error(`fallback_missing=${fallback.missingPaths.join(",")}`);
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
    console.error("trace=unavailable source=lexical_fallback");
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
