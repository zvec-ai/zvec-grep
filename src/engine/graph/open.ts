import { UnavailableGraphStorage } from "./unavailable.js";
import { SqliteGraphStorage } from "./sqlite.js";
import type { GraphStorage } from "./types.js";
import { EngineError, detail, errorDetails } from "../errors.js";

export type GraphBackend = "sqlite" | "off";

export type OpenGraphOptions = {
  backend?: GraphBackend;
  readOnly?: boolean;
};

/**
 * Open a graph store beside a collection.
 * Default backend is SQLite. Explicit `off` returns an unavailable stub.
 */
export function openGraphStorage(
  directory: string,
  options: OpenGraphOptions = {},
): GraphStorage {
  const backend = resolveBackend(options.backend);
  if (backend === "off") {
    return new UnavailableGraphStorage("graph backend is explicitly disabled");
  }

  try {
    const graph = new SqliteGraphStorage(directory, {
      readOnly: options.readOnly,
    });
    return graph;
  } catch (error) {
    const reason = graphOpenFailureReason(directory, error);
    if (options.readOnly) {
      return new UnavailableGraphStorage(reason);
    }
    throw new EngineError("Failed to open writable graph storage", {
      code: "ZVEC_GREP.ENGINE.GRAPH.OPEN_FAILED",
      context: errorDetails([
        detail("path", directory),
        detail("cause", error instanceof Error ? error.message : String(error)),
      ]),
      cause: error,
    });
  }
}

function graphOpenFailureReason(directory: string, error: unknown): string {
  const cause = error instanceof Error ? error.message : String(error);
  return `failed to open graph at ${directory}: ${cause}`;
}

function resolveBackend(explicit?: GraphBackend): GraphBackend {
  if (explicit) {
    return explicit;
  }
  const env = process.env.ZVEC_GREP_GRAPH_BACKEND?.trim().toLowerCase();
  if (env === "sqlite" || env === "off") {
    return env;
  }
  return "sqlite";
}
