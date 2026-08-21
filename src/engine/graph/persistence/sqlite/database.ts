import { mkdirSync } from "node:fs";
import { join } from "node:path";
import type { DatabaseSync as NodeDatabaseSync } from "node:sqlite";
import { loadNodeSqlite } from "./runtime.js";
import { SQLITE_GRAPH_SCHEMA, SQLITE_GRAPH_SCHEMA_VERSION } from "./schema.js";

export class SqliteGraphDatabase {
  readonly db: NodeDatabaseSync;
  readonly readOnly: boolean;
  private closed = false;

  constructor(
    directory: string,
    options: { readOnly?: boolean; inMemory?: boolean } = {},
  ) {
    const opened = openDatabase(directory, options);
    this.db = opened.db;
    this.readOnly = opened.readOnly;
  }

  close(): void {
    if (!this.closed) {
      this.db.close();
      this.closed = true;
    }
  }

  all<T>(sql: string, ...params: Array<string | number>): T[] {
    this.assertOpen();
    return this.db.prepare(sql).all(...params) as T[];
  }

  one<T>(sql: string, ...params: Array<string | number>): T | undefined {
    this.assertOpen();
    return this.db.prepare(sql).get(...params) as T | undefined;
  }

  transaction(work: () => void): void {
    this.assertWritable();
    this.db.exec("BEGIN IMMEDIATE");
    try {
      work();
      this.db.exec("COMMIT");
    } catch (error) {
      this.db.exec("ROLLBACK");
      throw error;
    }
  }

  assertOpen(): void {
    if (this.closed) throw new Error("SqliteGraphStorage is closed");
  }

  assertWritable(): void {
    this.assertOpen();
    if (this.readOnly) throw new Error("SqliteGraphStorage is read-only");
  }
}

function openDatabase(
  directory: string,
  options: { readOnly?: boolean; inMemory?: boolean } = {},
): { db: NodeDatabaseSync; readOnly: boolean } {
  const readOnly = options.readOnly ?? false;
  if (!options.inMemory && !readOnly) mkdirSync(directory, { recursive: true });
  const { DatabaseSync } = loadNodeSqlite();
  const db = new DatabaseSync(
    options.inMemory ? ":memory:" : join(directory, "graph.sqlite"),
    {
      readOnly,
      allowExtension: false,
      enableForeignKeyConstraints: true,
    },
  );
  try {
    if (readOnly) {
      if (!hasSchema(db)) {
        throw new Error("SQLite graph schema is missing");
      }
      ensureVersion(db, true);
    } else {
      db.exec("PRAGMA journal_mode=WAL");
      db.exec("PRAGMA synchronous=NORMAL");
      const existingSchema = hasSchema(db);
      if (existingSchema) ensureVersion(db, false);
      db.exec(SQLITE_GRAPH_SCHEMA);
      if (!existingSchema) ensureVersion(db, false);
    }
    return { db, readOnly };
  } catch (error) {
    try {
      db.close();
    } catch {
      // Preserve the initialization error; the handle is already unusable.
    }
    throw error;
  }
}

function hasSchema(db: NodeDatabaseSync): boolean {
  return (
    db
      .prepare(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='graph_meta'",
      )
      .get() !== undefined
  );
}

function ensureVersion(db: NodeDatabaseSync, readOnly: boolean): void {
  const row = db
    .prepare("SELECT value FROM graph_meta WHERE key='schema_version'")
    .get() as { value: string } | undefined;
  if (!row) {
    if (readOnly) throw new Error("SQLite graph schema version is missing");
    db.prepare(
      "INSERT INTO graph_meta(key,value) VALUES('schema_version',?)",
    ).run(String(SQLITE_GRAPH_SCHEMA_VERSION));
  } else if (Number(row.value) !== SQLITE_GRAPH_SCHEMA_VERSION) {
    throw new Error(
      `Unsupported SQLite graph schema version: ${row.value}; expected ${SQLITE_GRAPH_SCHEMA_VERSION}`,
    );
  }
}
