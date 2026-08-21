export const SQLITE_GRAPH_SCHEMA_VERSION = 1;

export const SQLITE_GRAPH_SCHEMA = `
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS graph_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS files (id TEXT PRIMARY KEY) STRICT;
CREATE TABLE IF NOT EXISTS symbols (
 id TEXT PRIMARY KEY, file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 name TEXT, kind TEXT NOT NULL, is_exported INTEGER NOT NULL CHECK (is_exported IN (0,1)),
 signature TEXT, arity INTEGER, return_type TEXT
) STRICT;
CREATE TABLE IF NOT EXISTS unresolved_refs (
 id TEXT PRIMARY KEY, owner_id TEXT NOT NULL,
 owner_is_file INTEGER NOT NULL CHECK (owner_is_file IN (0,1)),
 ref_name TEXT NOT NULL, ref_kind TEXT NOT NULL, line INTEGER NOT NULL,
 imported_name TEXT, local_name TEXT,
 source_language TEXT,
 receiver_kind TEXT, receiver_name TEXT, member_name TEXT, resolution_hints TEXT,
 status TEXT NOT NULL CHECK (status IN ('pending','failed','external','dynamic')),
 last_attempt INTEGER NOT NULL DEFAULT 0,
 dynamic_reason TEXT
) STRICT;
CREATE TABLE IF NOT EXISTS contains (
 parent_id TEXT NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
 child_id TEXT NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
 PRIMARY KEY(parent_id,child_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS edges (
 id TEXT PRIMARY KEY,
 src_id TEXT NOT NULL, dst_id TEXT NOT NULL,
 src_is_file INTEGER NOT NULL CHECK(src_is_file IN (0,1)),
 dst_is_file INTEGER NOT NULL CHECK(dst_is_file IN (0,1)),
 kind TEXT NOT NULL CHECK (kind IN ('CALLS','REFS','INHERITS','IMPORTS','INSTANTIATES')),
 rel TEXT NOT NULL, count INTEGER NOT NULL DEFAULT 1,
 first_line INTEGER NOT NULL DEFAULT 0, ref_name TEXT NOT NULL DEFAULT '',
 source_language TEXT, imported_name TEXT, local_name TEXT,
 receiver_kind TEXT, receiver_name TEXT, member_name TEXT, resolution_hints TEXT,
 provenance TEXT NOT NULL DEFAULT 'static' CHECK (provenance IN ('static','heuristic')),
 confidence REAL NOT NULL DEFAULT 1.0,
 evidence TEXT
) STRICT;
CREATE TABLE IF NOT EXISTS edge_candidates (
 edge_id TEXT NOT NULL REFERENCES unresolved_refs(id) ON DELETE CASCADE,
 target_id TEXT NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
 reason TEXT NOT NULL, confidence REAL NOT NULL,
 PRIMARY KEY(edge_id,target_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS symbols_file_id_idx ON symbols(file_id);
CREATE INDEX IF NOT EXISTS symbols_name_idx ON symbols(name) WHERE name IS NOT NULL;
CREATE INDEX IF NOT EXISTS edges_src_kind_idx ON edges(src_id,src_is_file,kind,dst_id);
CREATE INDEX IF NOT EXISTS edges_dst_kind_idx ON edges(dst_id,dst_is_file,kind,src_id);
CREATE INDEX IF NOT EXISTS edges_member_idx ON edges(member_name,kind);
CREATE INDEX IF NOT EXISTS contains_child_idx ON contains(child_id);
CREATE INDEX IF NOT EXISTS unresolved_refs_name_idx ON unresolved_refs(ref_name,status);
CREATE INDEX IF NOT EXISTS unresolved_refs_owner_idx ON unresolved_refs(owner_id,owner_is_file);
CREATE INDEX IF NOT EXISTS unresolved_refs_retry_idx ON unresolved_refs(ref_name,status,last_attempt,id);
CREATE INDEX IF NOT EXISTS unresolved_refs_member_idx ON unresolved_refs(member_name,status);
CREATE INDEX IF NOT EXISTS edge_candidates_target_idx ON edge_candidates(target_id);
`;
