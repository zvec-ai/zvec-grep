import { createRequire } from "node:module";
import type { DatabaseSync as NodeDatabaseSync } from "node:sqlite";

type NodeSqliteModule = { DatabaseSync: typeof NodeDatabaseSync };
const require = createRequire(import.meta.url);
const SQLITE_EXPERIMENTAL_WARNING =
  "SQLite is an experimental feature and might change at any time";

export function loadNodeSqlite(): NodeSqliteModule {
  const emitWarning = process.emitWarning;
  process.emitWarning = ((warning: string | Error, ...args: unknown[]) => {
    const message = warning instanceof Error ? warning.message : warning;
    if (message === SQLITE_EXPERIMENTAL_WARNING) return;
    Reflect.apply(emitWarning, process, [warning, ...args]);
  }) as typeof process.emitWarning;
  try {
    return require("node:sqlite") as NodeSqliteModule;
  } finally {
    process.emitWarning = emitWarning;
  }
}
