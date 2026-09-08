import { setTimeout as delay } from "node:timers/promises";
import { DaemonCallTimeoutError } from "./daemon-client.js";

// A first semantic-only query deserves a chance to finish a small cached-model
// build. Long downloads/builds remain background work rather than an unbounded
// foreground wait. Explicit --refresh wait is still the unbounded choice.
export const IMPLICIT_INDEX_WAIT_MS = 8_000;

type Readiness = "ready" | "pending" | "failed" | "disabled";
type ReadinessOptions = {
  budgetMs: number;
  status(timeoutMs: number): Promise<Record<string, unknown>>;
  onProgress?: (status: Record<string, unknown>) => void;
};

/** Retry with the persisted local model, never replace it or enable a remote one. */
export function implicitPreparationModel(
  status: Record<string, unknown>,
  defaultLocalModel: string,
): string | undefined {
  const persistent = status.persistent as
    | {
        workspace_index?: {
          embedding?: { provider: string; model: string } | null;
        };
      }
    | undefined;
  const embedding = persistent?.workspace_index?.embedding;
  if (!embedding) return defaultLocalModel;
  return embedding.provider === "local"
    ? `local/${embedding.model}`
    : undefined;
}

/** An initialized collection with only pending/failed files is not searchable. */
export function hasUsableImplicitIndex(
  status: Record<string, unknown>,
): boolean {
  if (status.indexed !== true || status.index_policy === "disabled")
    return false;
  const persistent = status.persistent as
    | {
        files?: {
          indexed: number;
          pending: number;
          failed: number;
          added: number;
        };
      }
    | undefined;
  const files = persistent?.files;
  // Keep a previously populated index usable during a refresh or per-file
  // failure. Freshness is handled by search, not by initial preparation.
  if (files && files.indexed > 0) return true;
  if (files && (files.pending > 0 || files.failed > 0 || files.added > 0))
    return false;
  const runtime = status.runtime as { job_state?: string } | undefined;
  return !["queued", "running", "failed", "cancelled"].includes(
    runtime?.job_state ?? "",
  );
}

export async function waitForImplicitIndex(
  options: ReadinessOptions,
  clock = { now: () => performance.now(), delay },
): Promise<Readiness> {
  const deadline = clock.now() + options.budgetMs;
  while (clock.now() < deadline) {
    let status: Record<string, unknown>;
    try {
      status = await options.status(Math.max(1, deadline - clock.now()));
    } catch (error) {
      if (error instanceof DaemonCallTimeoutError) return "pending";
      throw error;
    }
    if (status.index_policy === "disabled") return "disabled";
    const runtime = status.runtime as { job_state?: string } | undefined;
    if (runtime?.job_state === "failed" || runtime?.job_state === "cancelled")
      return "failed";
    const persistent = status.persistent as
      { files?: { indexed: number; failed: number } } | undefined;
    if (
      runtime?.job_state !== "queued" &&
      runtime?.job_state !== "running" &&
      persistent?.files?.indexed === 0 &&
      persistent.files.failed > 0
    )
      return "failed";
    // A manifest and native collection can exist before the initial job has
    // populated them. This poll follows initial preparation, not a refresh of
    // an already usable index, so wait for the job to finish before querying.
    if (
      hasUsableImplicitIndex(status) &&
      runtime?.job_state !== "queued" &&
      runtime?.job_state !== "running"
    )
      return "ready";
    options.onProgress?.(status);
    const remaining = deadline - clock.now();
    if (remaining > 0) await clock.delay(Math.min(200, remaining));
  }
  return "pending";
}
