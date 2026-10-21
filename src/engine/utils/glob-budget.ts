import { AsyncLocalStorage } from "node:async_hooks";

export const MAX_GLOB_PATTERN_CHARS = 4_096;
export const MAX_GLOB_PATH_CHARS = 32_768;
export const MAX_GLOB_RULES = 10_000;
/**
 * Upper bound on the combined compiled weight of every rule that may be
 * applied to one candidate path: built-in ignores, ignore-file rules, root
 * include/exclude patterns, request globs and expanded file types. Unlike the
 * matcher cache bound, exceeding this rejects the rule set itself.
 */
export const MAX_ACTIVE_RULE_WEIGHT = 250_000;

/**
 * Fixed per-path headroom. Must stay above the matcher's one-million-work
 * per-match cap so a single admitted pattern can complete (or reject through
 * its own cap) without tripping the path budget first.
 */
const PATH_WORK_BASE = 1_500_000;
/**
 * Legitimate matching work for one path grows with the path length times the
 * compiled weight of the active rules; the bounded NFA keeps actual work at
 * or below that linear bound, so this multiplier leaves headroom. The fixed
 * candidate ceiling provides the adversary-reachable cumulative cap;
 * yielding between rule chunks bounds event-loop block time.
 */
const PATH_WORK_MULTIPLIER = 4;

export class GlobWorkLimitError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "GlobWorkLimitError";
  }
}

type GlobBudgetStore = {
  sinceYield: number;
  pathRemaining?: number;
  overheadOnly?: boolean;
};

const operationBudget = new AsyncLocalStorage<GlobBudgetStore>();

/**
 * Hard ceiling on one candidate's cumulative matching work, independent of
 * path length or rule weight. Calibrated empirically: 20,000,000 rejects an
 * accepted long-path regression (~15M units) while 50,000,000 admits it;
 * ordinary repositories use roughly 1M per path. Supplements intra-candidate
 * yielding — it does not replace it.
 */
const MAX_CANDIDATE_WORK = 50_000_000;

/**
 * Keep concurrent searches/scans independent, and share a budget with nested work.
 */
export function withGlobBudget<T>(operation: () => T): T {
  if (operationBudget.getStore()) return operation();
  return operationBudget.run({ sinceYield: 0 }, operation);
}

export function globPathAllowance(
  pathLength: number,
  activeRuleWeight: number,
): number {
  return (
    PATH_WORK_BASE + (pathLength + 16) * activeRuleWeight * PATH_WORK_MULTIPLIER
  );
}

/**
 * Charge all rule checks for one candidate path against a shared allowance
 * sized by that path and the active rule weight, capped by the fixed
 * candidate ceiling. The body may be asynchronous: the allowance stays
 * active across awaited chunks and is restored only after the body settles,
 * keeping concurrent operations isolated. Yield and cancellation accounting
 * is unaffected; yields never replenish the allowance.
 */
export async function withGlobPathBudget<T>(
  pathLength: number,
  activeRuleWeight: number,
  body: () => T | Promise<T>,
): Promise<T> {
  const allowance = Math.min(
    globPathAllowance(pathLength, activeRuleWeight),
    MAX_CANDIDATE_WORK,
  );
  // Each candidate runs in its own store so interleaved candidates within one
  // enclosing operation can neither clobber nor inherit each other's
  // allowance; the save/restore pattern is only safe for strictly nested
  // calls, which concurrent candidates are not.
  const store = operationBudget.getStore();
  if (!store) {
    return operationBudget.run(
      { sinceYield: 0, pathRemaining: allowance },
      body,
    );
  }
  return operationBudget.run(
    {
      sinceYield: store.sinceYield,
      overheadOnly: store.overheadOnly,
      pathRemaining: allowance,
    },
    body,
  );
}

/**
 * Loading and compilation work (ignore-file reads, parse-time normalization
 * and validation, matcher compilation): counted for cooperative yielding
 * only, never debited from the operation pool or a path allowance. Bounded
 * by the per-input limits and active-rule admission instead.
 */
export function chargeGlobOverhead(work: number): void {
  const budget = operationBudget.getStore();
  if (budget) budget.sinceYield += work;
}

/**
 * Run rule loading/parsing with pool debits suspended; charges inside count
 * for yielding only. Per-candidate matching keeps its normal accounting.
 */
export function withGlobOverheadOnly<T>(body: () => T): T {
  const store = operationBudget.getStore();
  if (!store) return body();
  const previous = store.overheadOnly;
  store.overheadOnly = true;
  try {
    return body();
  } finally {
    store.overheadOnly = previous;
  }
}

export function chargeGlobWork(work: number): void {
  const budget = operationBudget.getStore();
  if (!budget) return;
  budget.sinceYield += work;
  if (budget.overheadOnly || budget.pathRemaining === undefined) return;
  if ((budget.pathRemaining -= work) < 0) {
    throw new GlobWorkLimitError(
      "Path filtering exceeded its matching work limit.",
    );
  }
}

/** Async callers poll between paths, never from inside synchronous matching. */
export function globWorkNeedsYield(): boolean {
  const budget = operationBudget.getStore();
  if (!budget || budget.sinceYield < 100_000) return false;
  budget.sinceYield = 0;
  return true;
}

/**
 * Yield the event loop between candidate rule chunks when the work counter
 * trips, checking cancellation first. Charges nothing and never replenishes
 * any allowance; one uninterrupted block stays near a single rule's cost.
 */
export async function yieldGlobWorkIfNeeded(
  yieldToEventLoop: () => Promise<unknown>,
  signal?: AbortSignal,
): Promise<void> {
  // Cancellation is checked on every call, regardless of the work threshold,
  // again immediately after yielding: a pre-aborted operation never starts
  // matching, and an abort during the final chunk cannot return success.
  throwIfAbortedSignal(signal);
  if (!globWorkNeedsYield()) return;
  await yieldToEventLoop();
  throwIfAbortedSignal(signal);
}

const cancellationErrors = new WeakSet<object>();

/** Preserve the signal's reason and mark it so pattern-error wrappers rethrow
 *  cancellation untouched instead of relabeling it as a pattern failure. */
export function globCancellationError(signal: AbortSignal): Error {
  const reason = signal.reason instanceof Error ? signal.reason : null;
  const error =
    reason ??
    (Object.assign(new Error("Indexing was cancelled."), {
      cause: signal.reason,
    }) as Error);
  // Mark via WeakSet: never mutate the reason, which callers may freeze.
  cancellationErrors.add(error);
  return error;
}

export function isGlobCancellation(error: unknown): boolean {
  return cancellationErrors.has(error as object);
}

export function checkGlobCancellation(signal?: AbortSignal): void {
  if (signal?.aborted) throw globCancellationError(signal);
}

function throwIfAbortedSignal(signal: AbortSignal | undefined): void {
  if (!signal?.aborted) return;
  throw globCancellationError(signal);
}

export function checkGlobLength(value: string, kind: "pattern" | "path"): void {
  const limit =
    kind === "pattern" ? MAX_GLOB_PATTERN_CHARS : MAX_GLOB_PATH_CHARS;
  if (value.length > limit) {
    throw new Error(`Glob ${kind} exceeds the ${limit}-character limit.`);
  }
}

export function checkGlobRuleCount(count: number): void {
  if (count > MAX_GLOB_RULES) {
    throw new Error(`Path filtering exceeds the ${MAX_GLOB_RULES}-rule limit.`);
  }
}

export function isGlobWorkLimitFailure(error: unknown): boolean {
  return (
    error instanceof GlobWorkLimitError ||
    (error instanceof Error && error.message.includes("matching work limit"))
  );
}

/** Attach a rule's provenance (label and pattern preview) to any pattern
 *  failure — work-limit exhaustion or compilation/matching errors — while
 *  preserving the original cause. Already-labeled errors pass through. */
const labeledGlobErrorMarker = Symbol("labeledGlobError");

export function labeledGlobError(
  label: string,
  pattern: string,
  cause: unknown,
): Error {
  if (!(cause instanceof Error)) {
    return new Error(`Glob pattern failed at ${label}: ${String(cause)}`);
  }
  if (isGlobCancellation(cause)) {
    return cause;
  }
  const marked = cause as { [labeledGlobErrorMarker]?: boolean };
  if (marked[labeledGlobErrorMarker]) {
    return cause;
  }
  const preview = pattern.length > 48 ? `${pattern.slice(0, 45)}…` : pattern;
  const error = new Error(
    isGlobWorkLimitFailure(cause)
      ? `Glob matching exceeded its work limit at ${label} (pattern '${preview}').`
      : `Glob pattern failed at ${label} (pattern '${preview}'): ${cause.message}`,
    { cause },
  );
  (error as { [labeledGlobErrorMarker]?: boolean })[labeledGlobErrorMarker] =
    true;
  return error;
}

export function checkActiveRuleWeight(weight: number, context: string): void {
  if (weight > MAX_ACTIVE_RULE_WEIGHT) {
    throw new Error(
      `Path filtering exceeds the ${MAX_ACTIVE_RULE_WEIGHT.toLocaleString("en-US")}-unit active-rule limit for ${context}.`,
    );
  }
}
