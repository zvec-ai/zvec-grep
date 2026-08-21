import { FilePathIndex } from "../../imports/path-index.js";
import { resolveImportPath } from "../../imports/resolve-path.js";
import { NameIndex } from "../../name-index.js";
import { referenceResolutionPolicy } from "../../reference-resolution-policy.js";
import { referenceTargetFromRaw } from "../../../reference-target.js";
import type { ReferenceResolutionHints } from "../../../reference-target.js";
import { resolveRef } from "../../resolve.js";
import type { PendingRef, ResolvePendingOptions } from "../../types.js";
import { type RefRow, type SymbolRow } from "./reader.js";
import type { SqliteGraphDatabase } from "./database.js";
import { SemanticCandidateRepository } from "./candidate-repository.js";
import { bareName } from "../../builtins.js";

const PER_NAME_CEILING = 500;
type ResolvePhase = "imports" | "inheritance" | "symbols";

/** Converts pending call/ref/import sites into persisted graph edges. */
export class SqlitePendingRefResolver {
  private readonly candidates: SemanticCandidateRepository;

  constructor(private readonly database: SqliteGraphDatabase) {
    this.candidates = new SemanticCandidateRepository(database);
  }

  private get db() {
    return this.database.db;
  }

  private assertWritable(): void {
    this.database.assertWritable();
  }

  private transaction(work: () => void): void {
    this.database.transaction(work);
  }

  private all<T>(sql: string, ...params: Array<string | number>): T[] {
    return this.database.all<T>(sql, ...params);
  }

  private one<T>(
    sql: string,
    ...params: Array<string | number>
  ): T | undefined {
    return this.database.one<T>(sql, ...params);
  }
  async resolvePending(options: ResolvePendingOptions = {}): Promise<void> {
    this.assertWritable();
    const resolvable =
      this.one<{ count: number }>(
        "SELECT COUNT(*) AS count FROM unresolved_refs WHERE status IN ('pending','failed')",
      )?.count ?? 0;
    if (resolvable === 0) return;
    const names = new NameIndex();
    const lookupNames = this.pendingSymbolNames();
    names.load(
      this.all<
        SymbolRow & {
          container_id: string | null;
          container_name: string | null;
        }
      >(
        `SELECT s.id,s.file_id,s.name,s.kind,s.is_exported,
                p.id AS container_id,p.name AS container_name
         FROM symbols s
         LEFT JOIN contains c ON c.child_id=s.id
         LEFT JOIN symbols p ON p.id=c.parent_id
         WHERE s.name IN (SELECT value FROM json_each(?))`,
        JSON.stringify(lookupNames),
      ).map((row) => ({
        id: row.id,
        fileId: row.file_id,
        name: row.name!,
        kind: row.kind,
        containerName: row.container_name ?? undefined,
        containerId: row.container_id ?? undefined,
      })),
    );
    const paths = new FilePathIndex(options.files ?? []);
    const attempt = this.nextAttempt();
    this.drainPhase("imports", attempt, (ref) =>
      this.resolveImport(ref, paths, attempt),
    );
    this.drainPhase("inheritance", attempt, (ref) =>
      this.resolveSymbol(ref, names, attempt, new Map()),
    );
    // Build/cache hierarchy lookups only after every inheritance batch has
    // completed, so calls never observe a partial inheritance graph.
    const hierarchyCache = new Map<string, readonly string[]>();
    this.drainPhase("symbols", attempt, (ref) =>
      this.resolveSymbol(ref, names, attempt, hierarchyCache),
    );
  }

  private pendingSymbolNames(): string[] {
    const rows = this.all<{
      ref_name: string;
      member_name: string | null;
      imported_name: string | null;
    }>(
      `SELECT ref_name,member_name,imported_name FROM unresolved_refs
       WHERE status IN ('pending','failed')`,
    );
    const names = new Set<string>();
    for (const row of rows) {
      for (const value of [row.ref_name, row.member_name, row.imported_name]) {
        if (!value) continue;
        names.add(value);
        const bare = bareName(value);
        if (bare) names.add(bare);
      }
    }
    for (const row of this.all<{ imported_name: string }>(
      `SELECT DISTINCT imports.imported_name FROM unresolved_refs unresolved
       JOIN symbols owner ON owner.id=unresolved.owner_id
       JOIN edges imports ON imports.src_id=owner.file_id
         AND imports.src_is_file=1 AND imports.kind='IMPORTS'
       WHERE unresolved.owner_is_file=0
         AND unresolved.status IN ('pending','failed')
         AND imports.imported_name IS NOT NULL`,
    )) {
      if (row.imported_name !== "*") names.add(row.imported_name);
    }
    return [...names];
  }

  private drainPhase(
    phase: ResolvePhase,
    attempt: number,
    resolve: (ref: RefRow) => void,
  ): void {
    const rounds = this.retryRounds(attempt, phase);
    for (let round = 0; round < rounds; round++) {
      this.transaction(() => {
        for (const ref of this.retryableRefs(attempt, phase)) resolve(ref);
      });
    }
  }

  private resolveSymbol(
    ref: RefRow,
    names: NameIndex,
    attempt: number,
    hierarchyCache: Map<string, readonly string[]>,
  ): void {
    const owner = this.one<{
      file_id: string;
      container_id: string | null;
      container_name: string | null;
    }>(
      `SELECT s.file_id,p.id AS container_id,p.name AS container_name FROM symbols s
       LEFT JOIN contains c ON c.child_id=s.id
       LEFT JOIN symbols p ON p.id=c.parent_id
       WHERE s.id=?`,
      ref.owner_id,
    );
    if (!owner) return this.failRef(ref.id, attempt);
    const pending: PendingRef = {
      src: ref.owner_id,
      src_file: owner.file_id,
      ref_id: ref.id,
      ref_name: ref.ref_name,
      ref_kind: ref.ref_kind,
      line: ref.line,
      status: ref.status,
      source_language: ref.source_language ?? undefined,
      target: {
        raw: ref.ref_name,
        member: ref.member_name ?? referenceTargetFromRaw(ref.ref_name).member,
        receiver:
          ref.receiver_kind && ref.receiver_name
            ? { kind: ref.receiver_kind, name: ref.receiver_name }
            : undefined,
        hints: parseResolutionHints(ref.resolution_hints),
      },
    };
    const reference = referenceResolutionPolicy.analyzeReference(
      pending.target ?? ref.ref_name,
      ref.source_language ?? undefined,
    );
    const preferred = this.all<{ dst_file_id: string }>(
      "SELECT DISTINCT dst_id AS dst_file_id FROM edges WHERE src_id=? AND src_is_file=1 AND dst_is_file=1 AND kind='IMPORTS'",
      owner.file_id,
    ).map((row) => row.dst_file_id);
    const binding = this.one<{
      imported_name: string;
      dst_file_id: string;
      local_name: string;
    }>(
      `SELECT imported_name,dst_id AS dst_file_id,local_name
       FROM edges
       WHERE src_is_file=1 AND src_id=? AND kind='IMPORTS'
         AND imported_name IS NOT NULL
         AND local_name IN (?,?)
       ORDER BY CASE WHEN local_name=? THEN 0 ELSE 1 END,
                dst_id,imported_name LIMIT 1`,
      owner.file_id,
      ref.ref_name,
      refReceiver(ref.ref_name),
      ref.ref_name,
    );
    const target = pending.target!;
    const semanticResolution = target.hints?.receiverType
      ? this.candidates.resolve({
          sourceId: ref.owner_id,
          sourceLanguage: ref.source_language ?? undefined,
          typeNames: target.hints.candidateTypes ?? [target.hints.receiverType],
          memberName: target.member,
          callArity: target.hints.callArity,
          limit: 65,
        })
      : { candidates: [], abstractDispatch: false, rtaActive: false };
    const semanticCandidates = semanticResolution.candidates;
    if (
      semanticCandidates.length === 1 &&
      (!semanticResolution.abstractDispatch || semanticResolution.rtaActive)
    ) {
      this.insertSymbolEdge(
        ref,
        semanticCandidates[0]!,
        refKindToEdgeKind(ref.ref_kind),
        {
          provenance: "heuristic",
          confidence: 0.75,
          evidence: "receiver_type_member",
        },
      );
      return;
    }
    if (
      ref.ref_kind === "call" &&
      ((semanticResolution.abstractDispatch && !semanticResolution.rtaActive) ||
        semanticCandidates.length > 1)
    ) {
      this.persistDynamicCall(ref, target, semanticCandidates);
      return;
    }
    const result = resolveRef(
      pending,
      names,
      binding ? [binding.dst_file_id] : preferred,
      binding
        ? {
            importedName: binding.imported_name,
            fileId: binding.dst_file_id,
            kind: binding.local_name === ref.ref_name ? "exact" : "receiver",
          }
        : undefined,
      owner.container_name ?? undefined,
      owner.container_id ?? undefined,
      reference.receiver.kind === "owner" && owner.container_id
        ? this.cachedInheritanceContainers(
            hierarchyCache,
            owner.container_id,
            reference.receiver.includeOwner,
          )
        : [],
      reference,
    );
    if (result.status === "external") {
      this.db
        .prepare("UPDATE unresolved_refs SET status='external' WHERE id=?")
        .run(ref.id);
      return;
    }
    if (result.status !== "resolved") {
      const heuristic = this.heuristicCandidate(
        ref,
        names,
        owner.file_id,
        binding ? [binding.dst_file_id] : preferred,
      );
      if (!heuristic) return this.failRef(ref.id, attempt);
      this.insertSymbolEdge(ref, heuristic, refKindToEdgeKind(ref.ref_kind), {
        provenance: "heuristic",
        confidence: 0.35,
        evidence: "unique_member_in_visible_files",
      });
      return;
    }
    this.insertSymbolEdge(ref, result.dst, result.edgeKind, {
      provenance: "static",
      confidence: 1,
      // A same-file resolution is derivable from the edge endpoints. Persist
      // only evidence that cannot be reconstructed from the graph itself.
      evidence: result.evidence === "same_file" ? undefined : result.evidence,
    });
  }

  private heuristicCandidate(
    ref: RefRow,
    names: NameIndex,
    sourceFileId: string,
    preferredFileIds: readonly string[],
  ): string | null {
    if (ref.receiver_kind !== "qualified" || !ref.member_name) return null;
    const candidates = names
      .candidates(ref.member_name, [sourceFileId, ...preferredFileIds])
      .filter(
        (entry) => entry.id !== ref.owner_id && entry.containerId !== undefined,
      );
    return candidates.length === 1 ? candidates[0]!.id : null;
  }

  private persistDynamicCall(
    ref: RefRow,
    target: NonNullable<PendingRef["target"]>,
    candidates: readonly string[],
  ): void {
    this.db
      .prepare(
        `UPDATE unresolved_refs
       SET status='dynamic',dynamic_reason='polymorphic_dispatch',
           member_name=?,receiver_kind=?,receiver_name=?,resolution_hints=?
       WHERE id=?`,
      )
      .run(
        target.member,
        target.receiver?.kind ?? null,
        target.receiver?.name ?? null,
        target.hints ? JSON.stringify(target.hints) : null,
        ref.id,
      );
    this.db.prepare("DELETE FROM edge_candidates WHERE edge_id=?").run(ref.id);
    const insert = this.db.prepare(
      "INSERT INTO edge_candidates(edge_id,target_id,reason,confidence) VALUES(?,?,?,?)",
    );
    const reason = target.hints?.genericBounds?.length
      ? "generic_bound"
      : "hierarchy";
    for (const candidate of candidates)
      insert.run(ref.id, candidate, reason, 0.65);
  }

  private insertSymbolEdge(
    ref: RefRow,
    dst: string,
    edgeKind: "CALLS" | "REFS" | "INHERITS",
    metadata: {
      provenance: "static" | "heuristic";
      confidence: number;
      evidence?: string;
    } = { provenance: "static", confidence: 1 },
  ): void {
    if (ref.ref_kind === "new") {
      this.db
        .prepare(
          `INSERT OR REPLACE INTO edges(
           id,src_id,dst_id,src_is_file,dst_is_file,kind,rel,count,first_line,
           ref_name,source_language,provenance,confidence
         ) VALUES(?,?,?,0,0,'INSTANTIATES','new',1,?,?,?,'static',1)`,
        )
        .run(
          `${ref.id}:instantiates`,
          ref.owner_id,
          dst,
          ref.line,
          ref.ref_name,
          ref.source_language,
        );
    }
    this.db
      .prepare(
        `INSERT OR REPLACE INTO edges(
           id,src_id,dst_id,src_is_file,dst_is_file,kind,rel,count,first_line,
           ref_name,source_language,imported_name,local_name,receiver_kind,
           receiver_name,member_name,resolution_hints,provenance,confidence,evidence
         ) VALUES(?,?,?,0,0,?,?,1,?,?,?,?,?,?,?,?,?,?,?,?)`,
      )
      .run(
        ref.id,
        ref.owner_id,
        dst,
        edgeKind,
        ref.ref_kind,
        ref.line,
        ref.ref_name,
        ref.source_language,
        ref.imported_name,
        ref.local_name,
        ref.receiver_kind,
        ref.receiver_name,
        ref.member_name,
        ref.resolution_hints,
        metadata.provenance,
        metadata.confidence,
        metadata.evidence ?? null,
      );
    this.db.prepare("DELETE FROM edge_candidates WHERE edge_id=?").run(ref.id);
    this.db.prepare("DELETE FROM unresolved_refs WHERE id=?").run(ref.id);
  }

  private resolveImport(
    ref: RefRow,
    paths: FilePathIndex,
    attempt: number,
  ): void {
    const from = paths.getById(ref.owner_id);
    if (!from) return this.failRef(ref.id, attempt);
    const result = resolveImportPath(
      ref.ref_name,
      ref.owner_id,
      from.format,
      paths,
    );
    if (result.status === "external") {
      this.db
        .prepare("UPDATE unresolved_refs SET status='external' WHERE id=?")
        .run(ref.id);
      return;
    }
    if (result.status !== "resolved") return this.failRef(ref.id, attempt);
    this.db
      .prepare(
        `INSERT OR REPLACE INTO edges(
         id,src_id,dst_id,src_is_file,dst_is_file,kind,rel,count,first_line,
         ref_name,source_language,imported_name,local_name,receiver_kind,
         receiver_name,member_name,resolution_hints,provenance,confidence,evidence
       ) VALUES(?,?,?,1,1,'IMPORTS','import',1,?,?,?,?,?,?,?,?,?,'static',1,NULL)`,
      )
      .run(
        ref.id,
        ref.owner_id,
        result.fileId,
        ref.line,
        ref.ref_name,
        ref.source_language,
        ref.imported_name,
        ref.local_name,
        ref.receiver_kind,
        ref.receiver_name,
        ref.member_name,
        ref.resolution_hints,
      );
    this.db.prepare("DELETE FROM unresolved_refs WHERE id=?").run(ref.id);
  }

  private retryableRefs(
    attemptWatermark: number,
    phase: ResolvePhase,
  ): RefRow[] {
    const phaseCondition = resolvePhaseCondition(phase);
    return this.all<RefRow>(
      `SELECT id,owner_id,owner_is_file,ref_name,ref_kind,line,status,imported_name,local_name,source_language,receiver_kind,receiver_name,member_name,resolution_hints,last_attempt
       FROM (
         SELECT unresolved_refs.*,
                row_number() OVER (PARTITION BY ref_name ORDER BY last_attempt,id) AS retry_rank
         FROM unresolved_refs
         WHERE status='failed' AND last_attempt<? AND ${phaseCondition}
       )
       WHERE retry_rank<=?
       UNION ALL
       SELECT id,owner_id,owner_is_file,ref_name,ref_kind,line,status,imported_name,local_name,source_language,receiver_kind,receiver_name,member_name,resolution_hints,last_attempt
       FROM unresolved_refs
       WHERE status='pending' AND last_attempt<? AND ${phaseCondition}
       ORDER BY ref_name,id`,
      attemptWatermark,
      PER_NAME_CEILING,
      attemptWatermark,
    );
  }

  private inheritanceContainers(
    containerId: string,
    includeOwner: boolean,
  ): string[] {
    return this.all<{ id: string; depth: number }>(
      `WITH RECURSIVE hierarchy(id,depth) AS (
         SELECT ?,0
         UNION
         SELECT e.dst_id,h.depth+1
         FROM edges e JOIN hierarchy h ON e.src_id=h.id
         WHERE e.kind='INHERITS'
           AND e.rel IN ('extends','implements')
           AND h.depth<32
       )
       SELECT id,depth FROM hierarchy WHERE depth>=? ORDER BY depth,id`,
      containerId,
      includeOwner ? 0 : 1,
    ).map((row) => row.id);
  }

  private cachedInheritanceContainers(
    cache: Map<string, readonly string[]>,
    containerId: string,
    includeOwner: boolean,
  ): readonly string[] {
    const key = `${containerId}\0${includeOwner ? "with-owner" : "bases-only"}`;
    const cached = cache.get(key);
    if (cached) return cached;
    const containers = this.inheritanceContainers(containerId, includeOwner);
    cache.set(key, containers);
    return containers;
  }

  private retryRounds(attemptWatermark: number, phase: ResolvePhase): number {
    const phaseCondition = resolvePhaseCondition(phase);
    const row = this.one<{ max_count: number }>(
      `SELECT COALESCE(MAX(ref_count),0) AS max_count FROM (
         SELECT COUNT(*) AS ref_count FROM unresolved_refs
         WHERE status='failed' AND last_attempt<? AND ${phaseCondition}
         GROUP BY ref_name
       )`,
      attemptWatermark,
    );
    return Math.max(1, Math.ceil((row?.max_count ?? 0) / PER_NAME_CEILING));
  }

  private failRef(id: string, attempt: number): void {
    this.db
      .prepare(
        "UPDATE unresolved_refs SET status='failed',last_attempt=? WHERE id=?",
      )
      .run(attempt, id);
  }

  private nextAttempt(): number {
    const row = this.db
      .prepare(
        `INSERT INTO graph_meta(key,value) VALUES('pending_ref_attempt','1')
         ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1
         RETURNING value`,
      )
      .get() as { value: string };
    return Number(row.value);
  }
}

function resolvePhaseCondition(phase: ResolvePhase): string {
  if (phase === "imports") return "(owner_is_file=1 OR ref_kind='import')";
  if (phase === "inheritance")
    return "owner_is_file=0 AND ref_kind IN ('extends','implements','overrides')";
  return "owner_is_file=0 AND ref_kind NOT IN ('import','extends','implements','overrides')";
}

function refReceiver(name: string): string {
  return name.split(/[./]/, 1)[0] ?? name;
}

function refKindToEdgeKind(kind: string): "CALLS" | "REFS" | "INHERITS" {
  if (kind === "call") return "CALLS";
  if (kind === "extends" || kind === "implements" || kind === "overrides")
    return "INHERITS";
  return "REFS";
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
