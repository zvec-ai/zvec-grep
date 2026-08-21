import type { DatabaseSync as NodeDatabaseSync } from "node:sqlite";
import type { ReferenceResolutionHints } from "../../../reference-target.js";
import { SqliteGraphDatabase } from "./database.js";
import { SemanticCandidateRepository } from "./candidate-repository.js";
import type {
  ContainerNeighbor,
  FileNeighbor,
  GraphEdge,
  GraphEdgeKind,
  DynamicBoundary,
  GraphStats,
  InducedEdgesResult,
  SeedNeighbor,
  SymContext,
  SymRef,
  TraverseOpts,
  UsageRef,
} from "../../types.js";

export type EdgeRow = {
  src_id: string;
  dst_id: string;
  kind: "CALLS" | "REFS" | "INHERITS" | "IMPORTS" | "INSTANTIATES";
  rel: string;
  count: number;
  first_line: number;
  ref_name: string;
  provenance: "static" | "heuristic";
  confidence: number;
  evidence: string | null;
};

const MAX_TRAVERSAL_EDGE_READ = 10_000;
export type RefRow = {
  id: string;
  owner_id: string;
  owner_is_file: number;
  ref_name: string;
  ref_kind: string;
  line: number;
  status: "pending" | "failed" | "external";
  imported_name: string | null;
  local_name: string | null;
  source_language: string | null;
  receiver_kind: "owner" | "super" | "qualified" | null;
  receiver_name: string | null;
  member_name: string | null;
  resolution_hints: string | null;
  last_attempt: number;
};
export type SymbolRow = {
  id: string;
  file_id: string;
  name: string | null;
  kind: string;
  is_exported: number;
};
const REL_KINDS = new Set<GraphEdgeKind>(["CALLS", "REFS", "INHERITS"]);
const ALL_EDGE_KINDS: readonly GraphEdgeKind[] = [
  "CALLS",
  "REFS",
  "INHERITS",
  "CONTAINS",
  "DEFINES",
  "IMPORTS",
  "INSTANTIATES",
];

/** Indexed SQLite graph reader without a full-memory mirror. */
export class SqliteGraphReader {
  private readonly candidates: SemanticCandidateRepository;
  readonly available = true;
  protected readonly db: NodeDatabaseSync;
  protected readonly readOnly: boolean;
  protected readonly database: SqliteGraphDatabase;

  constructor(
    directory: string | SqliteGraphDatabase,
    options: { readOnly?: boolean; inMemory?: boolean } = {},
  ) {
    this.database =
      directory instanceof SqliteGraphDatabase
        ? directory
        : new SqliteGraphDatabase(directory, options);
    this.db = this.database.db;
    this.readOnly = this.database.readOnly;
    this.candidates = new SemanticCandidateRepository(this.database);
  }
  close(): void {
    this.database.close();
  }

  dynamicBoundaries(
    nodeIds: readonly string[],
    limit: number,
  ): DynamicBoundary[] {
    if (nodeIds.length === 0 || limit <= 0) return [];
    const ids = [...new Set(nodeIds)];
    const placeholders = ids.map(() => "?").join(",");
    const persisted = this.all<{
      id: string;
      owner_id: string;
      ref_name: string;
      member_name: string;
      receiver_kind: "owner" | "super" | "qualified" | null;
      receiver_name: string | null;
      resolution_hints: string | null;
      reason: "polymorphic_dispatch";
    }>(
      `SELECT id,owner_id,ref_name,member_name,receiver_kind,receiver_name,
              resolution_hints,dynamic_reason AS reason
       FROM unresolved_refs unresolved
       WHERE status='dynamic' AND ref_kind='call'
         AND owner_id IN (${placeholders})
       ORDER BY EXISTS(
         SELECT 1 FROM edge_candidates candidate
         WHERE candidate.edge_id=unresolved.id
       ) DESC,owner_id,line,id LIMIT ?`,
      ...ids,
      limit,
    ).map((row): DynamicBoundary => {
      const candidateRows = this.all<{
        target_id: string;
        reason: "hierarchy" | "generic_bound" | "method_set";
        confidence: number;
      }>(
        `SELECT target_id,reason,confidence FROM edge_candidates
         WHERE edge_id=? ORDER BY confidence DESC,target_id LIMIT 65`,
        row.id,
      );
      const details = candidateRows.slice(0, 64).map((candidate) => ({
        targetId: candidate.target_id,
        reason: candidate.reason,
        confidence: candidate.confidence,
      }));
      return {
        sourceId: row.owner_id,
        target: {
          raw: row.ref_name,
          member: row.member_name,
          receiver:
            row.receiver_kind && row.receiver_name
              ? { kind: row.receiver_kind, name: row.receiver_name }
              : undefined,
          ...resolutionHintsField(row.resolution_hints),
        },
        reason: row.reason,
        candidates: details.map((candidate) => candidate.targetId),
        candidatesTruncated: candidateRows.length > 64,
        candidateDetails: details,
      };
    });
    const remaining = Math.max(0, limit - persisted.length);
    if (remaining === 0) return persisted;
    const unresolved = this.all<RefRow>(
      `SELECT id,owner_id,owner_is_file,ref_name,ref_kind,line,status,imported_name,local_name,source_language,receiver_kind,receiver_name,member_name,resolution_hints,last_attempt
       FROM unresolved_refs
       WHERE owner_is_file=0 AND status='failed' AND ref_kind='call'
         AND receiver_kind IS NOT NULL
         AND owner_id IN (${placeholders})
       ORDER BY owner_id,line,id LIMIT ?`,
      ...ids,
      remaining,
    ).map((row): DynamicBoundary => {
      const hints = parseResolutionHints(row.resolution_hints);
      const target = {
        raw: row.ref_name,
        member: row.member_name ?? row.ref_name,
        receiver:
          row.receiver_kind && row.receiver_name
            ? { kind: row.receiver_kind, name: row.receiver_name }
            : undefined,
        ...resolutionHintsField(row.resolution_hints),
      } as DynamicBoundary["target"];
      const candidateRows = hints?.receiverType
        ? this.candidates.findConcrete({
            sourceId: row.owner_id,
            sourceLanguage: row.source_language ?? undefined,
            typeNames: hints.candidateTypes ?? [hints.receiverType],
            memberName: target.member,
            callArity: hints.callArity,
            limit: 65,
          })
        : [];
      const candidates = candidateRows.slice(0, 64);
      return {
        sourceId: row.owner_id,
        target,
        reason:
          candidates.length > 1 || hints?.dispatch
            ? "polymorphic_dispatch"
            : row.receiver_kind === "owner" || row.receiver_kind === "super"
              ? "polymorphic_dispatch"
              : "unknown_receiver_type",
        candidates,
        candidatesTruncated: candidateRows.length > 64,
        candidateDetails: candidates.map((targetId) => ({
          targetId,
          reason: hints?.genericBounds?.length
            ? ("generic_bound" as const)
            : ("hierarchy" as const),
          confidence: 0.5,
        })),
      };
    });
    return [...persisted, ...unresolved];
  }

  symbolScope(root: string, depth: number, limit: number): string[] {
    return this.traverse(root, {
      edgeKinds: ["CALLS", "REFS"],
      direction: "both",
      maxDepth: depth,
      limit,
    }).map((r) => r.id);
  }
  fileScope(fileId: string, depth: number, limit: number): string[] {
    return this.bfs(fileId, ["IMPORTS"], "both", depth, limit).map((r) => r.id);
  }

  expandSeeds(symIds: readonly string[], limit: number): SeedNeighbor[] {
    const out: SeedNeighbor[] = [];
    for (const sid of symIds) {
      for (const edge of this.adjacentEdges([sid], ["CALLS"], "both", limit)) {
        out.push({
          sid,
          id: edge.src === sid ? edge.dst : edge.src,
          count: edge.count,
          direction: edge.src === sid ? "out" : "in",
        });
      }
    }
    return out;
  }

  expandContainers(
    symIds: readonly string[],
    limit: number,
  ): ContainerNeighbor[] {
    const out: ContainerNeighbor[] = [];
    for (const sid of symIds) {
      const parent = this.one<{ parent_id: string }>(
        "SELECT parent_id FROM contains WHERE child_id=?",
        sid,
      )?.parent_id;
      if (!parent) continue;
      const sibs = this.all<{ child_id: string }>(
        "SELECT child_id FROM contains WHERE parent_id=? AND child_id<>? LIMIT ?",
        parent,
        sid,
        limit,
      );
      if (sibs.length === 0) out.push({ sid, parent_id: parent, sib_id: null });
      else
        for (const s of sibs)
          out.push({ sid, parent_id: parent, sib_id: s.child_id });
    }
    return out;
  }

  expandFileNeighbors(
    fileIds: readonly string[],
    limit: number,
  ): FileNeighbor[] {
    const out: FileNeighbor[] = [];
    for (const fid of fileIds) {
      for (const edge of this.adjacentEdges(
        [fid],
        ["IMPORTS"],
        "both",
        limit,
      )) {
        out.push({
          fid,
          id: edge.src === fid ? edge.dst : edge.src,
          direction: edge.src === fid ? "out" : "in",
        });
      }
    }
    return out;
  }

  callers(id: string, depth: number, limit: number): SymRef[] {
    return this.bfs(id, ["CALLS"], "incoming", depth, limit);
  }
  callees(id: string, depth: number, limit: number): SymRef[] {
    if (depth <= 1)
      return this.all<EdgeRow>(
        `SELECT src_id,dst_id,kind,rel,SUM(count) AS count,
                MIN(first_line) AS first_line,MIN(ref_name) AS ref_name,
                CASE WHEN MAX(provenance)='static' THEN 'static' ELSE 'heuristic' END AS provenance,
                MAX(confidence) AS confidence,
                CASE WHEN MAX(provenance)='static' THEN NULL ELSE MIN(evidence) END AS evidence
         FROM edges WHERE src_id=? AND src_is_file=0 AND kind='CALLS'
         GROUP BY src_id,dst_id,kind,rel ORDER BY count DESC,dst_id LIMIT ?`,
        id,
        limit,
      ).map((e) => ({
        id: e.dst_id,
        kind: this.symbolKind(e.dst_id),
        count: e.count,
      }));
    return this.bfs(id, ["CALLS"], "outgoing", depth, limit);
  }
  impact(id: string, depth: number, limit: number): SymRef[] {
    return this.bfs(id, ["CALLS", "REFS"], "incoming", depth, limit);
  }
  usages(id: string, limit: number): UsageRef[] {
    return this.all<EdgeRow>(
      `SELECT src_id,dst_id,kind,rel,SUM(count) AS count,
              MIN(first_line) AS first_line,MIN(ref_name) AS ref_name,
              CASE WHEN MAX(provenance)='static' THEN 'static' ELSE 'heuristic' END AS provenance,
              MAX(confidence) AS confidence,
              CASE WHEN MAX(provenance)='static' THEN NULL ELSE MIN(evidence) END AS evidence
       FROM edges WHERE dst_id=? AND dst_is_file=0
       GROUP BY src_id,dst_id,kind,rel ORDER BY first_line LIMIT ?`,
      id,
      limit,
    ).map((e) => ({
      id: e.src_id,
      rel: e.rel,
      first_line: e.first_line,
      count: e.count,
    }));
  }

  pathBetween(
    from: string,
    to: string,
    maxDepth: number,
    edgeLimit = 10_000,
  ): SymRef[] | null {
    if (from === to) return [{ id: from, kind: this.symbolKind(from) }];
    let remainingEdges = Math.max(0, Math.floor(edgeLimit));
    if (remainingEdges === 0) return null;
    const parent = new Map<string, string | null>([[from, null]]);
    let frontier = [from];
    for (
      let depth = 0;
      depth < clampDepth(maxDepth) && frontier.length;
      depth++
    ) {
      const next: string[] = [];
      const edges = this.adjacentEdges(
        frontier,
        ["CALLS"],
        "outgoing",
        remainingEdges,
      );
      remainingEdges -= edges.length;
      for (const edge of edges) {
        if (parent.has(edge.dst)) continue;
        parent.set(edge.dst, edge.src);
        if (edge.dst === to) return this.reconstructPath(parent, to);
        next.push(edge.dst);
      }
      if (remainingEdges <= 0) return null;
      frontier = next;
    }
    return null;
  }

  hierarchy(
    id: string,
    direction: "bases" | "derived",
    limit: number,
  ): SymRef[] {
    return this.bfs(
      id,
      ["INHERITS"],
      direction === "bases" ? "outgoing" : "incoming",
      10,
      limit,
    );
  }
  members(id: string): SymRef[] {
    return this.all<{ id: string; kind: string }>(
      "SELECT s.id,s.kind FROM contains c JOIN symbols s ON s.id=c.child_id WHERE c.parent_id=?",
      id,
    );
  }
  deadCode(limit: number): SymRef[] {
    return this.all<{ id: string; kind: string }>(
      `SELECT s.id,s.kind FROM symbols s WHERE s.is_exported=0 AND s.kind IN ('function','method') AND NOT EXISTS(SELECT 1 FROM edges e WHERE e.dst_id=s.id AND e.dst_is_file=0 AND e.kind='CALLS') LIMIT ?`,
      limit,
    );
  }

  context(id: string): SymContext {
    const containers: SymRef[] = [];
    let current = id;
    for (let i = 0; i < 5; i++) {
      const p = this.one<{ parent_id: string }>(
        "SELECT parent_id FROM contains WHERE child_id=?",
        current,
      );
      if (!p) break;
      containers.push({ id: p.parent_id, kind: this.symbolKind(p.parent_id) });
      current = p.parent_id;
    }
    const outgoing = this.outgoingEdges(
      [id],
      ["CALLS", "REFS", "INHERITS", "INSTANTIATES"],
      100,
    ).map((edge) => ({ id: edge.dst, rel: edge.rel }));
    return {
      focal: { id, kind: this.symbolKind(id) },
      containers,
      members: this.members(id),
      incoming: this.usages(id, 100),
      outgoing,
    };
  }

  traverse(id: string, opts: TraverseOpts): SymRef[] {
    const found = this.bfs(
      id,
      opts.edgeKinds,
      opts.direction,
      opts.maxDepth,
      opts.limit,
    );
    return opts.includeStart
      ? [{ id, kind: this.symbolKind(id) }, ...found].slice(0, opts.limit)
      : found;
  }

  outgoingEdges(
    nodeIds: readonly string[],
    edgeKinds: readonly GraphEdgeKind[] = ALL_EDGE_KINDS,
    limit = 1_000,
  ): GraphEdge[] {
    return this.queryDirectionalEdges(nodeIds, edgeKinds, "outgoing", limit);
  }

  incomingEdges(
    nodeIds: readonly string[],
    edgeKinds: readonly GraphEdgeKind[] = ALL_EDGE_KINDS,
    limit = 1_000,
  ): GraphEdge[] {
    return this.queryDirectionalEdges(nodeIds, edgeKinds, "incoming", limit);
  }

  edges(
    nodeIds: readonly string[],
    edgeKinds: readonly GraphEdgeKind[],
    limit: number,
  ): InducedEdgesResult {
    const budget = Math.max(0, Math.floor(limit));
    if (nodeIds.length === 0 || budget === 0)
      return { edges: [], truncated: false };
    const ids = JSON.stringify([...new Set(nodeIds)]);
    const selects: string[] = [];
    const params: (string | number)[] = [];
    const rel = edgeKinds.filter((k) => REL_KINDS.has(k));
    if (rel.length) {
      const p = rel.map(() => "?").join(",");
      selects.push(
        `SELECT src_id AS src,dst_id AS dst,kind,rel,SUM(count) AS count,
                MIN(first_line) AS first_line,MIN(ref_name) AS ref_name,
                CASE WHEN MAX(provenance)='static' THEN 'static' ELSE 'heuristic' END AS provenance,
                MAX(confidence) AS confidence,
                CASE WHEN MAX(provenance)='static' THEN NULL ELSE MIN(evidence) END AS evidence
         FROM edges
         WHERE src_id IN(SELECT value FROM json_each(?))
           AND dst_id IN(SELECT value FROM json_each(?)) AND kind IN(${p})
         GROUP BY src_id,dst_id,kind,rel`,
      );
      params.push(ids, ids, ...rel);
    }
    if (edgeKinds.includes("CONTAINS")) {
      selects.push(
        `SELECT parent_id AS src,child_id AS dst,'CONTAINS' AS kind,
                'contains' AS rel,1 AS count,0 AS first_line,'' AS ref_name,
                'static' AS provenance,1.0 AS confidence,NULL AS evidence
         FROM contains
         WHERE parent_id IN(SELECT value FROM json_each(?))
           AND child_id IN(SELECT value FROM json_each(?))`,
      );
      params.push(ids, ids);
    }
    if (edgeKinds.includes("DEFINES")) {
      selects.push(
        `SELECT file_id AS src,id AS dst,'DEFINES' AS kind,
                'defines' AS rel,1 AS count,0 AS first_line,'' AS ref_name,
                'static' AS provenance,1.0 AS confidence,NULL AS evidence
         FROM symbols
         WHERE file_id IN(SELECT value FROM json_each(?))
           AND id IN(SELECT value FROM json_each(?))`,
      );
      params.push(ids, ids);
    }
    if (edgeKinds.includes("IMPORTS")) {
      selects.push(
        `SELECT src_id AS src,dst_id AS dst,kind,rel,SUM(count) AS count,
                MIN(first_line) AS first_line,MIN(ref_name) AS ref_name,
                CASE WHEN MAX(provenance)='static' THEN 'static' ELSE 'heuristic' END AS provenance,
                MAX(confidence) AS confidence,
                CASE WHEN MAX(provenance)='static' THEN NULL ELSE MIN(evidence) END AS evidence
         FROM edges
         WHERE kind='IMPORTS' AND src_is_file=1 AND dst_is_file=1
           AND src_id IN(SELECT value FROM json_each(?))
           AND dst_id IN(SELECT value FROM json_each(?))
         GROUP BY src_id,dst_id,kind,rel`,
      );
      params.push(ids, ids);
    }
    if (edgeKinds.includes("INSTANTIATES")) {
      selects.push(
        `SELECT src_id AS src,dst_id AS dst,kind,rel,SUM(count) AS count,
                MIN(first_line) AS first_line,MIN(ref_name) AS ref_name,
                CASE WHEN MAX(provenance)='static' THEN 'static' ELSE 'heuristic' END AS provenance,
                MAX(confidence) AS confidence,
                CASE WHEN MAX(provenance)='static' THEN NULL ELSE MIN(evidence) END AS evidence
         FROM edges
         WHERE kind='INSTANTIATES' AND src_is_file=0 AND dst_is_file=0
           AND src_id IN(SELECT value FROM json_each(?))
           AND dst_id IN(SELECT value FROM json_each(?))
         GROUP BY src_id,dst_id,kind,rel`,
      );
      params.push(ids, ids);
    }
    if (selects.length === 0) return { edges: [], truncated: false };

    const rows = this.all<{
      src: string;
      dst: string;
      kind: GraphEdgeKind;
      rel: string;
      count: number;
      first_line: number;
      ref_name: string;
      provenance: "static" | "heuristic";
      confidence: number;
      evidence?: string;
    }>(
      `SELECT * FROM (${selects.join(" UNION ALL ")})
       ORDER BY kind,src,dst,rel LIMIT ?`,
      ...params,
      budget + 1,
    );
    return {
      edges: rows.slice(0, budget),
      truncated: rows.length > budget,
    };
  }

  stats(): GraphStats {
    const count = (table: string, where = "") =>
      Number(
        this.one<{ count: number }>(
          `SELECT count(*) count FROM ${table} ${where}`,
        )?.count ?? 0,
      );
    const unresolvedCounts = new Map(
      this.all<{ status: string; count: number }>(
        "SELECT status,COUNT(*) AS count FROM unresolved_refs GROUP BY status",
      ).map((row) => [row.status, Number(row.count)]),
    );
    const pendingRefCount = unresolvedCounts.get("pending") ?? 0;
    const failedRefCount = unresolvedCounts.get("failed") ?? 0;
    return {
      symCount: count("symbols"),
      fileCount: count("files"),
      refCount: pendingRefCount + failedRefCount,
      pendingRefCount,
      failedRefCount,
      dynamicBoundaryCount: unresolvedCounts.get("dynamic") ?? 0,
      externalRefCount: unresolvedCounts.get("external") ?? 0,
      callsCount: this.edgeOccurrenceCount("CALLS"),
      refsCount: this.edgeOccurrenceCount("REFS"),
      inheritsCount: this.edgeOccurrenceCount("INHERITS"),
    };
  }

  private bfs(
    start: string,
    kinds: readonly GraphEdgeKind[],
    direction: "outgoing" | "incoming" | "both",
    maxDepth: number,
    limit: number,
  ): SymRef[] {
    if (limit <= 0) return [];
    const seen = new Set([start]),
      ordered: string[] = [];
    let frontier = [start];
    for (
      let depth = 0;
      depth < clampDepth(maxDepth) && frontier.length;
      depth++
    ) {
      const next: string[] = [];
      const active = new Set(frontier);
      const remaining = Math.max(0, limit - ordered.length);
      let edgeBudget = Math.min(
        MAX_TRAVERSAL_EDGE_READ,
        Math.max(1, remaining),
      );
      while (next.length < remaining) {
        const adjacent = this.adjacentEdges(
          frontier,
          kinds,
          direction,
          edgeBudget,
        );
        for (const edge of adjacent)
          for (const id of adjacentTargets(edge, active, direction)) {
            if (seen.has(id)) continue;
            seen.add(id);
            ordered.push(id);
            next.push(id);
            if (ordered.length >= Math.max(0, limit))
              return this.refsForIds(ordered);
          }
        if (
          adjacent.length < edgeBudget ||
          edgeBudget >= MAX_TRAVERSAL_EDGE_READ
        )
          break;
        edgeBudget = Math.min(MAX_TRAVERSAL_EDGE_READ, edgeBudget * 2);
      }
      frontier = next;
    }
    return this.refsForIds(ordered);
  }

  private adjacentEdges(
    idsInput: readonly string[],
    kinds: readonly GraphEdgeKind[],
    direction: "outgoing" | "incoming" | "both",
    limit: number,
  ): GraphEdge[] {
    if (direction === "outgoing") {
      return this.outgoingEdges(idsInput, kinds, limit);
    }
    if (direction === "incoming") {
      return this.incomingEdges(idsInput, kinds, limit);
    }
    const outgoing = this.outgoingEdges(idsInput, kinds, Math.ceil(limit / 2));
    const incoming = this.incomingEdges(
      idsInput,
      kinds,
      Math.max(0, limit - outgoing.length),
    );
    return dedupeEdges([...outgoing, ...incoming]).slice(0, limit);
  }

  private queryDirectionalEdges(
    idsInput: readonly string[],
    kinds: readonly GraphEdgeKind[],
    direction: "outgoing" | "incoming",
    limit: number,
  ): GraphEdge[] {
    if (!idsInput.length || limit <= 0) return [];
    const ids = JSON.stringify([...new Set(idsInput)]);
    const requested = [...new Set(kinds)];
    if (requested.length === 0) return [];
    const quota = Math.max(1, Math.ceil(limit / requested.length));
    const buckets = new Map<GraphEdgeKind, GraphEdge[]>();
    const exhausted = new Set<GraphEdgeKind>();
    for (const kind of requested) {
      const rows = this.queryDirectionalEdgeKind(
        ids,
        kind,
        direction,
        quota,
        0,
      );
      buckets.set(kind, rows);
      if (rows.length < quota) exhausted.add(kind);
    }
    let out = roundRobinEdges(requested, buckets, limit);
    while (out.length < limit) {
      let progressed = false;
      for (const kind of requested) {
        if (exhausted.has(kind)) continue;
        const bucket = buckets.get(kind)!;
        const rows = this.queryDirectionalEdgeKind(
          ids,
          kind,
          direction,
          quota,
          bucket.length,
        );
        bucket.push(...rows);
        progressed ||= rows.length > 0;
        if (rows.length < quota) exhausted.add(kind);
      }
      out = roundRobinEdges(requested, buckets, limit);
      if (!progressed) break;
    }
    return out;
  }

  private queryDirectionalEdgeKind(
    ids: string,
    kind: GraphEdgeKind,
    direction: "outgoing" | "incoming",
    limit: number,
    offset: number,
  ): GraphEdge[] {
    if (REL_KINDS.has(kind) || kind === "INSTANTIATES" || kind === "IMPORTS") {
      const side = direction === "outgoing" ? "src_id" : "dst_id";
      const fileFlags =
        kind === "IMPORTS"
          ? "src_is_file=1 AND dst_is_file=1"
          : "src_is_file=0 AND dst_is_file=0";
      return this.all<EdgeRow>(
        `SELECT src_id,dst_id,kind,rel,SUM(count) AS count,
                MIN(first_line) AS first_line,MIN(ref_name) AS ref_name,
                CASE WHEN MAX(provenance)='static' THEN 'static' ELSE 'heuristic' END AS provenance,
                MAX(confidence) AS confidence,
                CASE WHEN MAX(provenance)='static' THEN NULL ELSE MIN(evidence) END AS evidence
         FROM edges WHERE kind=? AND ${fileFlags}
           AND ${side} IN(SELECT value FROM json_each(?))
         GROUP BY src_id,dst_id,kind,rel
         ORDER BY ${side},src_id,dst_id,rel LIMIT ? OFFSET ?`,
        kind,
        ids,
        limit,
        offset,
      ).map(toGraphEdge);
    }
    if (kind === "CONTAINS") {
      const side = direction === "outgoing" ? "parent_id" : "child_id";
      return this.all<{ parent_id: string; child_id: string }>(
        `SELECT parent_id,child_id FROM contains
         WHERE ${side} IN(SELECT value FROM json_each(?))
         ORDER BY ${side},parent_id,child_id LIMIT ? OFFSET ?`,
        ids,
        limit,
        offset,
      ).map((row) =>
        structuralEdge(row.parent_id, row.child_id, "CONTAINS", "contains"),
      );
    }
    const side = direction === "outgoing" ? "file_id" : "id";
    return this.all<{ file_id: string; id: string }>(
      `SELECT file_id,id FROM symbols
       WHERE ${side} IN(SELECT value FROM json_each(?))
       ORDER BY ${side},file_id,id LIMIT ? OFFSET ?`,
      ids,
      limit,
      offset,
    ).map((row) => structuralEdge(row.file_id, row.id, "DEFINES", "defines"));
  }

  private edgeOccurrenceCount(kind: "CALLS" | "REFS" | "INHERITS"): number {
    return Number(
      this.one<{ count: number }>(
        "SELECT COALESCE(SUM(count),0) AS count FROM edges WHERE kind=?",
        kind,
      )?.count ?? 0,
    );
  }

  private refsForIds(ids: readonly string[]): SymRef[] {
    if (!ids.length) return [];
    const kinds = new Map(
      this.all<{ id: string; kind: string }>(
        "SELECT id,kind FROM symbols WHERE id IN(SELECT value FROM json_each(?))",
        JSON.stringify(ids),
      ).map((r) => [r.id, r.kind]),
    );
    return ids.map((id) => ({ id, kind: kinds.get(id) }));
  }

  private reconstructPath(
    parent: Map<string, string | null>,
    end: string,
  ): SymRef[] {
    const ids: string[] = [];
    let cur: string | null = end;
    while (cur) {
      ids.push(cur);
      cur = parent.get(cur) ?? null;
    }
    return this.refsForIds(ids.reverse());
  }
  private symbolKind(id: string): string | undefined {
    return this.one<{ kind: string }>("SELECT kind FROM symbols WHERE id=?", id)
      ?.kind;
  }
  protected transaction(work: () => void): void {
    this.database.transaction(work);
  }
  protected all<T>(sql: string, ...params: Array<string | number>): T[] {
    return this.database.all<T>(sql, ...params);
  }
  protected one<T>(
    sql: string,
    ...params: Array<string | number>
  ): T | undefined {
    return this.database.one<T>(sql, ...params);
  }
  protected assertOpen(): void {
    this.database.assertOpen();
  }
  protected assertWritable(): void {
    this.database.assertWritable();
  }
}

function toGraphEdge(r: EdgeRow): GraphEdge {
  return {
    src: r.src_id,
    dst: r.dst_id,
    kind: r.kind,
    rel: r.rel,
    count: r.count,
    first_line: r.first_line,
    ref_name: r.ref_name,
    provenance: r.provenance ?? "static",
    confidence: r.confidence ?? 1,
    evidence: r.evidence ?? undefined,
  };
}

function parseResolutionHints(
  value: string | null,
): ReferenceResolutionHints | undefined {
  if (!value) return undefined;
  try {
    return JSON.parse(value) as ReferenceResolutionHints;
  } catch {
    return undefined;
  }
}

function resolutionHintsField(value: string | null): {
  hints?: ReferenceResolutionHints;
} {
  const hints = parseResolutionHints(value);
  return hints ? { hints } : {};
}
function structuralEdge(
  src: string,
  dst: string,
  kind: GraphEdgeKind,
  rel: string,
): GraphEdge {
  return {
    src,
    dst,
    kind,
    rel,
    count: 1,
    first_line: 0,
    ref_name: rel,
    provenance: "static",
    confidence: 1,
  };
}
function dedupeEdges(edges: readonly GraphEdge[]): GraphEdge[] {
  const seen = new Set<string>();
  return edges.filter((e) => {
    const k = `${e.src}\0${e.dst}\0${e.kind}\0${e.rel}`;
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}
function roundRobinEdges(
  kinds: readonly GraphEdgeKind[],
  buckets: ReadonlyMap<GraphEdgeKind, readonly GraphEdge[]>,
  limit: number,
): GraphEdge[] {
  const out: GraphEdge[] = [];
  for (let index = 0; out.length < limit; index++) {
    let added = false;
    for (const kind of kinds) {
      const edge = buckets.get(kind)?.[index];
      if (!edge) continue;
      out.push(edge);
      added = true;
      if (out.length >= limit) break;
    }
    if (!added) break;
  }
  return out;
}
function adjacentTargets(
  edge: GraphEdge,
  active: ReadonlySet<string>,
  direction: "outgoing" | "incoming" | "both",
): string[] {
  const out: string[] = [];
  if (direction !== "incoming" && active.has(edge.src)) out.push(edge.dst);
  if (direction !== "outgoing" && active.has(edge.dst)) out.push(edge.src);
  return out;
}
function clampDepth(n: number): number {
  return Math.max(0, Math.min(32, Math.floor(n)));
}
