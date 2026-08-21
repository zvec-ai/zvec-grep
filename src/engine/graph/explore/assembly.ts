import type { Range } from "../../types.js";
import type { GraphQueryStorage } from "../ports.js";
import type {
  ExploreFileBundle,
  ExploreNode,
  ExploreSymbolSnippet,
} from "./types.js";

const CLUSTER_GAP_LINES = 3;

export function assembleExploreFiles(input: {
  storage: GraphQueryStorage;
  nodes: readonly ExploreNode[];
  fileScores: Map<string, number>;
  maxFiles: number;
  maxChars: number;
  rootFileIds: ReadonlySet<string>;
  changeSurfaceFileIds: ReadonlySet<string>;
}): ExploreFileBundle[] {
  const byFile = new Map<string, ExploreNode[]>();
  for (const node of input.nodes) {
    const file = node.entity?.file;
    if (!file || !input.fileScores.has(file.id)) {
      continue;
    }
    const list = byFile.get(file.id) ?? [];
    list.push(node);
    byFile.set(file.id, list);
  }

  const rankedFileIds = [...input.fileScores.entries()]
    .filter(([fileId]) => byFile.has(fileId))
    .sort((a, b) => {
      const priority = (id: string): number =>
        input.rootFileIds.has(id)
          ? 0
          : input.changeSurfaceFileIds.has(id)
            ? 1
            : 2;
      const priorityDiff = priority(a[0]) - priority(b[0]);
      if (priorityDiff !== 0) return priorityDiff;
      if (b[1] !== a[1]) {
        return b[1] - a[1];
      }
      return a[0].localeCompare(b[0]);
    })
    .slice(0, input.maxFiles)
    .map(([fileId]) => fileId);

  if (rankedFileIds.length === 0) {
    return [];
  }

  const topScore = input.fileScores.get(rankedFileIds[0]!) ?? 0;
  const central = new Set(
    rankedFileIds
      .filter((id) => (input.fileScores.get(id) ?? 0) >= topScore * 0.85)
      .slice(0, 2),
  );

  const budgets = allocateCharBudgets(rankedFileIds, central, input.maxChars);

  const bundles: ExploreFileBundle[] = [];
  for (const fileId of rankedFileIds) {
    const nodes = byFile.get(fileId) ?? [];
    const file = nodes[0]?.entity?.file;
    if (!file) {
      continue;
    }
    const symbols = nodes
      .map((node) => toSymbolSnippet(node))
      .filter((s): s is ExploreSymbolSnippet => s !== null)
      .sort((a, b) => startLine(a.range) - startLine(b.range));

    const clustered = clusterSymbols(symbols);
    const budget =
      budgets.get(fileId) ?? Math.floor(input.maxChars / rankedFileIds.length);
    const text = renderFileText(clustered, budget);
    if (!text.trim()) {
      continue;
    }
    bundles.push({
      file,
      score: input.fileScores.get(fileId) ?? 0,
      isCentral: central.has(fileId),
      isChangeSurface: input.changeSurfaceFileIds.has(fileId),
      symbols: clustered,
      text,
    });
  }
  return bundles;
}

function toSymbolSnippet(node: ExploreNode): ExploreSymbolSnippet | null {
  const entity = node.entity;
  if (!entity || entity.entity.content.kind !== "text") {
    return null;
  }
  const meta = entity.entity.metadata;
  const name =
    meta?.kind === "code" && meta.symbolName
      ? meta.symbolName
      : node.id.slice(0, 12);
  return {
    id: node.id,
    name,
    kind: meta?.kind === "code" ? meta.symbolType : node.kind,
    range: entity.entity.range,
    content: entity.entity.content.text,
  };
}

function clusterSymbols(
  symbols: readonly ExploreSymbolSnippet[],
): ExploreSymbolSnippet[] {
  // Keep symbol identity; clustering only affects render adjacency.
  // Nearby symbols stay in order — merge is representational in renderFileText.
  return [...symbols];
}

function renderFileText(
  symbols: readonly ExploreSymbolSnippet[],
  budget: number,
): string {
  if (budget <= 0) return "";
  const parts: string[] = [];
  let used = 0;
  let prevEnd = -1;

  for (const sym of symbols) {
    const start = startLine(sym.range);
    const end = endLine(sym.range);
    const header = `// ${sym.kind ?? "sym"} ${sym.name} L${start}-${end}`;
    const gap =
      prevEnd >= 0 && start > prevEnd + CLUSTER_GAP_LINES ? "\n// ...\n" : "\n";
    const block = `${parts.length === 0 ? "" : gap}${header}\n${sym.content.trimEnd()}\n`;
    if (used + block.length > budget) {
      const remaining = budget - used;
      if (remaining > 0) {
        const marker = "\n// ... truncated\n";
        const contentLength = Math.max(0, remaining - marker.length);
        parts.push(
          `${block.slice(0, contentLength)}${marker.slice(0, remaining - contentLength)}`,
        );
      }
      break;
    }
    parts.push(block);
    used += block.length;
    prevEnd = end;
  }
  return parts.join("").slice(0, budget);
}

function allocateCharBudgets(
  fileIds: readonly string[],
  central: ReadonlySet<string>,
  maxChars: number,
): Map<string, number> {
  const budgets = new Map<string, number>();
  if (fileIds.length === 0) {
    return budgets;
  }
  const centralIds = fileIds.filter((id) => central.has(id));
  const otherIds = fileIds.filter((id) => !central.has(id));
  if (centralIds.length === 0 || otherIds.length === 0) {
    for (const id of fileIds) {
      budgets.set(id, Math.floor(maxChars / fileIds.length));
    }
    return budgets;
  }
  const centralShare = centralIds.length > 0 ? 0.55 : 0;
  const centralBudget = Math.floor(maxChars * centralShare);
  const otherBudget = maxChars - centralBudget;

  for (const id of centralIds) {
    budgets.set(id, Math.floor(centralBudget / centralIds.length));
  }
  for (const id of otherIds) {
    budgets.set(id, Math.floor(otherBudget / Math.max(1, otherIds.length)));
  }
  return budgets;
}

function startLine(range: Range): number {
  return range.kind === "text" ? range.startLine : 1;
}

function endLine(range: Range): number {
  return range.kind === "text" ? range.endLine : startLine(range);
}
