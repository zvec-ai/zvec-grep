import type { SqliteGraphDatabase } from "./database.js";

export type SemanticCandidateQuery = {
  sourceId: string;
  sourceLanguage?: string;
  typeNames: readonly string[];
  memberName: string;
  callArity?: number;
  limit?: number;
};

export type SemanticCandidate = {
  id: string;
  containerKind: string;
  abstractDispatch: boolean;
  rtaActive: boolean;
};

export type SemanticCandidateResolution = {
  candidates: string[];
  abstractDispatch: boolean;
  rtaActive: boolean;
};

/** Shared semantic-member lookup used by projection and read models. */
export class SemanticCandidateRepository {
  constructor(private readonly database: SqliteGraphDatabase) {}

  find(query: SemanticCandidateQuery): string[] {
    return [
      ...new Set(this.findDetailed(query).map((candidate) => candidate.id)),
    ];
  }

  findConcrete(query: SemanticCandidateQuery): string[] {
    return this.resolve(query).candidates;
  }

  resolve(query: SemanticCandidateQuery): SemanticCandidateResolution {
    const detailed = this.findDetailed(query);
    return {
      candidates: [
        ...new Set(
          detailed
            .filter(
              (candidate) => !isAbstractContainerKind(candidate.containerKind),
            )
            .map((candidate) => candidate.id),
        ),
      ],
      abstractDispatch: detailed.some(
        (candidate) => candidate.abstractDispatch,
      ),
      rtaActive: detailed.some((candidate) => candidate.rtaActive),
    };
  }

  private findDetailed(query: SemanticCandidateQuery): SemanticCandidate[] {
    const policy = candidatePolicy(query.sourceLanguage);
    return this.database
      .all<{
        id: string;
        container_kind: string;
        abstract_dispatch: number;
        rta_active: number;
      }>(
        `WITH RECURSIVE visible(file_id) AS (
         SELECT file_id FROM symbols WHERE id=?
         UNION SELECT imports.dst_id FROM edges imports
         JOIN symbols source ON source.file_id=imports.src_id
         WHERE source.id=? AND imports.kind='IMPORTS'
           AND imports.src_is_file=1 AND imports.dst_is_file=1
       ), roots(id,kind) AS (
         SELECT id,kind FROM symbols
         WHERE name IN (SELECT value FROM json_each(?))
           AND file_id IN (SELECT file_id FROM visible)
       ), required_interfaces(id) AS (
         SELECT id FROM roots WHERE kind IN (SELECT value FROM json_each(?))
         UNION
         SELECT inheritance.dst_id FROM edges inheritance
         JOIN required_interfaces required ON required.id=inheritance.src_id
         JOIN symbols inherited ON inherited.id=inheritance.dst_id
         WHERE inheritance.kind='INHERITS'
           AND inheritance.rel IN (SELECT value FROM json_each(?))
           AND inherited.kind IN (SELECT value FROM json_each(?))
       ), containers(id) AS (
         SELECT id FROM roots
         UNION
         SELECT e.src_id FROM edges e JOIN containers c ON c.id=e.dst_id
         WHERE e.kind='INHERITS'
           AND e.rel IN (SELECT value FROM json_each(?))
       ), provider_roots(id) AS (
         SELECT id FROM containers
         UNION
         SELECT id FROM symbols
         WHERE file_id IN (SELECT file_id FROM visible)
           AND kind IN ('class','interface','trait','abstract_class')
       ), provider_closure(container_id,provider_id,depth,path) AS (
         SELECT id,id,0,',' || id || ',' FROM provider_roots
         UNION ALL
         SELECT provider.container_id,inheritance.dst_id,provider.depth+1,
                provider.path || inheritance.dst_id || ','
         FROM provider_closure provider
         JOIN edges inheritance ON inheritance.src_id=provider.provider_id
         WHERE inheritance.kind='INHERITS'
           AND inheritance.rel IN (SELECT value FROM json_each(?))
           AND provider.depth<32
           AND instr(provider.path,',' || inheritance.dst_id || ',')=0
       ), candidate_containers(id) AS (
         SELECT id FROM containers
         UNION
         SELECT DISTINCT candidate.id
         FROM symbols candidate
         WHERE candidate.file_id IN (SELECT file_id FROM visible)
           AND candidate.kind NOT IN ('interface','trait')
           AND EXISTS(
             SELECT 1 FROM roots
             WHERE kind IN (SELECT value FROM json_each(?))
           )
           AND NOT EXISTS(
             SELECT 1 FROM required_interfaces required_interface
             JOIN contains required_owned
               ON required_owned.parent_id=required_interface.id
             JOIN symbols required ON required.id=required_owned.child_id
             WHERE NOT EXISTS(
                 SELECT 1 FROM provider_closure provider
                 JOIN contains provided_owned
                   ON provided_owned.parent_id=provider.provider_id
                 JOIN symbols provided ON provided.id=provided_owned.child_id
                 WHERE provider.container_id=candidate.id
                   AND provided.name=required.name
                   AND (required.arity IS NULL OR provided.arity IS NULL
                        OR provided.arity=required.arity)
               )
           )
       ), candidate_members(id,container_id,container_kind) AS (
         SELECT DISTINCT member.id,scope.id,scope_symbol.kind
         FROM candidate_containers scope
         JOIN symbols scope_symbol ON scope_symbol.id=scope.id
         JOIN provider_closure provider ON provider.container_id=scope.id
         JOIN symbols provider_symbol ON provider_symbol.id=provider.provider_id
         JOIN contains owned ON owned.parent_id=provider.provider_id
         JOIN symbols member ON member.id=owned.child_id
         WHERE member.name=? AND (?<0 OR member.arity IS NULL OR member.arity=?)
           AND NOT(provider_symbol.kind='abstract_class'
             AND scope_symbol.kind<>'abstract_class')
       )
       SELECT id,container_kind,
         EXISTS(SELECT 1 FROM roots
           WHERE kind IN ('interface','trait','abstract_class'))
           AS abstract_dispatch,
         EXISTS(
           SELECT 1 FROM candidate_members candidate
           JOIN edges made ON made.dst_id=candidate.container_id
           WHERE made.kind='INSTANTIATES' AND made.dst_is_file=0
         ) AS rta_active
       FROM candidate_members
       WHERE NOT EXISTS(
         SELECT 1 FROM candidate_members candidate
         JOIN edges made ON made.dst_id=candidate.container_id
         WHERE made.kind='INSTANTIATES' AND made.dst_is_file=0
       ) OR container_id IN (
         SELECT dst_id FROM edges WHERE kind='INSTANTIATES' AND dst_is_file=0
       )
       ORDER BY id LIMIT ?`,
        query.sourceId,
        query.sourceId,
        JSON.stringify([...new Set(query.typeNames)]),
        JSON.stringify(policy.structuralRootKinds),
        JSON.stringify(policy.inheritanceRelations),
        JSON.stringify(policy.structuralRootKinds),
        JSON.stringify(policy.inheritanceRelations),
        JSON.stringify(policy.providerRelations),
        JSON.stringify(policy.structuralRootKinds),
        query.memberName,
        query.callArity ?? -1,
        query.callArity ?? -1,
        query.limit ?? 64,
      )
      .map((row) => ({
        id: row.id,
        containerKind: row.container_kind,
        abstractDispatch: row.abstract_dispatch === 1,
        rtaActive: row.rta_active === 1,
      }));
  }
}

function isAbstractContainerKind(kind: string): boolean {
  return kind === "interface" || kind === "trait" || kind === "abstract_class";
}

function candidatePolicy(language?: string): {
  inheritanceRelations: readonly string[];
  structuralRootKinds: readonly string[];
  providerRelations: readonly string[];
} {
  if (language === "go")
    return {
      inheritanceRelations: ["implements", "extends"],
      structuralRootKinds: ["interface"],
      providerRelations: ["extends"],
    };
  if (language === "rust")
    return {
      inheritanceRelations: ["trait", "implements"],
      structuralRootKinds: [],
      providerRelations: [],
    };
  if (language === "java")
    return {
      inheritanceRelations: ["extends", "implements"],
      structuralRootKinds: [],
      providerRelations: ["extends"],
    };
  return {
    inheritanceRelations: ["extends", "implements", "trait"],
    structuralRootKinds: [],
    providerRelations: [],
  };
}
