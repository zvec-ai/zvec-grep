import type {
  ZvecGrepExploreResult,
  ZvecGrepGraphEntity,
  ZvecGrepGraphNeighborhoodResult,
} from "../engine/service/index.js";

type ExploreOutput = Omit<ZvecGrepExploreResult, "root"> & { root?: string };
type NeighborhoodOutput = Omit<ZvecGrepGraphNeighborhoodResult, "root"> & {
  root?: string;
};

export function printExploreResult(result: ExploreOutput): void {
  for (const line of exploreLines(result)) {
    console.log(line);
  }
}

export function formatExploreResult(result: ExploreOutput): string {
  return exploreLines(result).join("\n");
}

function exploreLines(result: ExploreOutput): string[] {
  if (!result.available) {
    return [
      result.unavailableReason
        ? `graph unavailable: ${result.unavailableReason}`
        : "graph unavailable",
    ];
  }
  if (result.emptyReason === "no_seeds") {
    return [`no seeds for query: ${result.query}`];
  }
  if (result.files.length === 0) {
    return [`no explore context for query: ${result.query}`];
  }

  const lines: string[] = [];
  lines.push(`explore: ${result.query}`);
  if (result.root) {
    lines.push(`root: ${result.root}`);
  }
  lines.push(
    `roots: ${result.roots.map((r) => symbolLabel(r.id, r.entity)).join(", ")}`,
  );
  lines.push(
    `subgraph: ${result.nodes.length} nodes, ${result.edges.length} edges${result.edgesTruncated ? " (truncated)" : ""}, ${result.files.length} files`,
  );

  if (result.callPaths.length > 0) {
    lines.push("", "call paths:");
    for (const path of result.callPaths) {
      lines.push(
        `- ${path.nodes.map((id) => shortName(result, id)).join(" -> ")}`,
      );
    }
  }

  const blast = blastRadiusLines(result);
  if (blast.length > 0) {
    lines.push("", "blast radius:", ...blast);
  }

  if (result.changeSurface.length > 0) {
    lines.push("", "change surface:");
    for (const item of result.changeSurface) {
      lines.push(
        `- ${shortName(result, item.rootId)} ${item.rel} -> ${symbolLabel(item.id, item.entity)}${item.rescued ? " (rescued)" : ""}`,
      );
    }
  }

  if ((result.dynamicBoundaries?.length ?? 0) > 0) {
    lines.push(
      "",
      `dynamic boundaries:${result.dynamicBoundariesTruncated ? " (truncated)" : ""}`,
    );
    for (const boundary of result.dynamicBoundaries ?? []) {
      lines.push(
        `- ${shortName(result, boundary.sourceId)} -> ${boundary.target.raw} (${boundary.reason}${boundary.candidateDetails.length > 0 ? `; candidates=${boundary.candidateDetails.map((candidate) => `${shortName(result, candidate.targetId)}@${candidate.confidence.toFixed(2)}`).join(",")}${boundary.candidatesTruncated ? ",..." : ""}` : ""})`,
      );
    }
  }

  for (const file of result.files) {
    lines.push("");
    const tag = file.isCentral
      ? "central"
      : file.isChangeSurface
        ? "change-surface"
        : "related";
    lines.push(
      `${file.file.relativePath} (${tag}, score=${file.score.toFixed(4)})`,
    );
    const relations = relationNotes(result, file.file.id);
    if (relations.length > 0) {
      lines.push(`relations: ${relations.join("; ")}`);
    }
    lines.push("source:");
    for (const textLine of file.text.split(/\r?\n/)) {
      lines.push(textLine);
    }
  }
  return lines;
}

function blastRadiusLines(result: ExploreOutput): string[] {
  const lines: string[] = [];
  for (const blast of result.blastRadius) {
    if (blast.dependents.length === 0 && blast.tests.length === 0) continue;
    lines.push(`- ${shortName(result, blast.rootId)}:`);
    if (blast.dependents.length > 0) {
      lines.push(
        `  dependents: ${blast.dependents
          .slice(0, 10)
          .map((item) => symbolLabel(item.id, item.entity))
          .join(", ")}`,
      );
    }
    if (blast.tests.length > 0) {
      lines.push(
        `  tests: ${blast.tests
          .slice(0, 10)
          .map((item) => symbolLabel(item.id, item.entity))
          .join(", ")}`,
      );
    }
  }
  return lines;
}

export function printNeighborhoodResult(result: NeighborhoodOutput): void {
  for (const line of neighborhoodLines(result)) {
    console.log(line);
  }
}

export function formatNeighborhoodResult(result: NeighborhoodOutput): string {
  return neighborhoodLines(result).join("\n");
}

function neighborhoodLines(result: NeighborhoodOutput): string[] {
  if (!result.available) {
    return [
      result.unavailableReason
        ? `graph unavailable: ${result.unavailableReason}`
        : "graph unavailable",
    ];
  }
  if (result.ambiguous) {
    const lines = [`ambiguous seeds for ${result.query}:`];
    for (const seed of result.seeds) {
      lines.push(`- ${symbolLabel(seed.id, seed.entity)}`);
    }
    lines.push("re-run with a unique name or --seed-id <id>");
    return lines;
  }
  if (!result.seed) {
    return [`no seeds for query: ${result.query}`];
  }

  const lines: string[] = [
    `${result.direction}: ${symbolLabel(result.seed.id, result.seed.entity)}`,
  ];
  if (result.root) {
    lines.push(`root: ${result.root}`);
  }
  lines.push(`depth=${result.depth} limit=${result.limit}`);
  if (result.neighbors.length === 0) {
    lines.push("(no neighbors)");
    return lines;
  }
  for (const neighbor of result.neighbors) {
    const count =
      neighbor.count !== undefined ? ` count=${neighbor.count}` : "";
    const kind = neighbor.kind ? ` ${neighbor.kind}` : "";
    lines.push(`- ${symbolLabel(neighbor.id, neighbor.entity)}${kind}${count}`);
  }
  return lines;
}

function relationNotes(result: ExploreOutput, fileId: string): string[] {
  const idsInFile = new Set(
    result.nodes.filter((n) => n.entity?.file.id === fileId).map((n) => n.id),
  );
  const notes: string[] = [];
  for (const edge of result.edges) {
    const srcIn = idsInFile.has(edge.src);
    const dstIn = idsInFile.has(edge.dst);
    if (!srcIn && !dstIn) {
      continue;
    }
    if (srcIn && dstIn) {
      notes.push(relationNote(result, edge));
    } else if (srcIn) {
      notes.push(relationNote(result, edge));
    } else {
      notes.push(relationNote(result, edge));
    }
    if (notes.length >= 8) {
      break;
    }
  }
  return notes;
}

function relationNote(
  result: ExploreOutput,
  edge: ExploreOutput["edges"][number],
): string {
  const certainty =
    edge.provenance === "heuristic"
      ? `? confidence=${edge.confidence.toFixed(2)}`
      : "";
  return `${shortName(result, edge.src)} -${edge.kind}${certainty}-> ${shortName(result, edge.dst)}`;
}

function shortName(result: ExploreOutput, id: string): string {
  const node = result.nodes.find((n) => n.id === id);
  return symbolLabel(id, node?.entity ?? null, true);
}

function symbolLabel(
  id: string,
  entity: ZvecGrepGraphEntity | null | undefined,
  short = false,
): string {
  const name = entity?.name ?? id.slice(0, 10);
  if (short) {
    return name;
  }
  const path = entity?.file.relativePath;
  return path ? `${name} (${path})` : name;
}
