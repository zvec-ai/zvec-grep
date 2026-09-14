import { AsyncLocalStorage } from "node:async_hooks";

export const MAX_GLOB_PATTERN_CHARS = 4_096;
export const MAX_GLOB_PATH_CHARS = 32_768;
export const MAX_GLOB_RULES = 10_000;
const MAX_OPERATION_WORK = 100_000_000;
const operationBudget = new AsyncLocalStorage<{
  remaining: number;
  sinceYield: number;
}>();

/** Keep concurrent searches/scans independent, and share a budget with nested work. */
export function withGlobBudget<T>(operation: () => T): T {
  if (operationBudget.getStore()) return operation();
  return operationBudget.run(
    { remaining: MAX_OPERATION_WORK, sinceYield: 0 },
    operation,
  );
}

export function chargeGlobWork(work: number): void {
  const budget = operationBudget.getStore();
  if (budget) budget.sinceYield += work;
  if (budget && (budget.remaining -= work) < 0) {
    throw new Error("Path filtering exceeded its matching work limit.");
  }
}

/** Async callers poll between paths, never from inside synchronous matching. */
export function globWorkNeedsYield(): boolean {
  const budget = operationBudget.getStore();
  if (!budget || budget.sinceYield < 100_000) return false;
  budget.sinceYield = 0;
  return true;
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
