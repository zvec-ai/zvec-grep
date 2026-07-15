import { performance } from "node:perf_hooks";
import type { TimingEntry } from "../types.js";

type MutableTimingEntry = {
  name: string;
  durationMs: number;
  count: number;
};

export class TimingCollector {
  private readonly entriesByName = new Map<string, MutableTimingEntry>();

  async time<T>(name: string, task: () => Promise<T>): Promise<T> {
    const start = performance.now();
    try {
      return await task();
    } finally {
      this.add(name, performance.now() - start);
    }
  }

  timeSync<T>(name: string, task: () => T): T {
    const start = performance.now();
    try {
      return task();
    } finally {
      this.add(name, performance.now() - start);
    }
  }

  add(name: string, durationMs: number, count = 1): void {
    const normalizedName = name.trim();
    if (!normalizedName || !Number.isFinite(durationMs)) {
      return;
    }

    const existing = this.entriesByName.get(normalizedName);
    if (existing) {
      existing.durationMs += Math.max(0, durationMs);
      existing.count += Math.max(1, count);
      return;
    }

    this.entriesByName.set(normalizedName, {
      name: normalizedName,
      durationMs: Math.max(0, durationMs),
      count: Math.max(1, count),
    });
  }

  addEntries(entries: readonly TimingEntry[] | undefined, prefix = ""): void {
    for (const entry of entries ?? []) {
      this.add(`${prefix}${entry.name}`, entry.durationMs, entry.count ?? 1);
    }
  }

  entries(): TimingEntry[] {
    return [...this.entriesByName.values()].map((entry) => ({
      name: entry.name,
      durationMs: Math.round(entry.durationMs),
      ...(entry.count > 1 ? { count: entry.count } : {}),
    }));
  }
}

export class ConcurrentTiming {
  private active = 0;
  private startedAt = 0;

  constructor(
    private readonly collector: TimingCollector,
    private readonly name: string,
  ) {}

  async time<T>(task: () => Promise<T>): Promise<T> {
    this.start();
    try {
      return await task();
    } finally {
      this.stop();
    }
  }

  private start(): void {
    if (this.active === 0) {
      this.startedAt = performance.now();
    }

    this.active++;
  }

  private stop(): void {
    if (this.active <= 0) {
      return;
    }

    this.active--;
    if (this.active === 0) {
      this.collector.add(this.name, performance.now() - this.startedAt);
      this.startedAt = 0;
    }
  }
}
