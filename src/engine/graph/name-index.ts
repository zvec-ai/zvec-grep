import { bareName } from "./builtins.js";
import type { ResolutionEvidence } from "./types.js";

export type NameEntry = {
  id: string;
  fileId: string;
  name: string;
  kind: string;
  containerName?: string;
  containerId?: string;
};

export type NameLookupResult = {
  entry: NameEntry;
  evidence: ResolutionEvidence;
};

/** Exact / bare-name lookup used by resolvePending. */
export class NameIndex {
  private readonly byName = new Map<string, NameEntry[]>();
  private readonly byId = new Map<string, NameEntry>();

  clear(): void {
    this.byName.clear();
    this.byId.clear();
  }

  removeFile(fileId: string): void {
    for (const [id, entry] of [...this.byId]) {
      if (entry.fileId === fileId) {
        this.removeId(id);
      }
    }
  }

  upsert(entries: readonly NameEntry[]): void {
    for (const entry of entries) {
      this.removeId(entry.id);
      this.byId.set(entry.id, entry);
      const list = this.byName.get(entry.name) ?? [];
      list.push(entry);
      this.byName.set(entry.name, list);
      const bare = bareName(entry.name);
      if (bare && bare !== entry.name) {
        const bareList = this.byName.get(bare) ?? [];
        bareList.push(entry);
        this.byName.set(bare, bareList);
      }
    }
  }

  lookup(
    refName: string,
    srcFile: string,
    preferredFileIds: readonly string[] = [],
    allowBareFallback = true,
    containerNames: readonly string[] = [],
    containerIds: readonly string[] = [],
  ): NameEntry | null {
    return (
      this.lookupWithEvidence(
        refName,
        srcFile,
        preferredFileIds,
        allowBareFallback,
        containerNames,
        containerIds,
      )?.entry ?? null
    );
  }

  lookupWithEvidence(
    refName: string,
    srcFile: string,
    preferredFileIds: readonly string[] = [],
    allowBareFallback = true,
    containerNames: readonly string[] = [],
    containerIds: readonly string[] = [],
  ): NameLookupResult | null {
    let candidates =
      this.byName.get(refName) ??
      (allowBareFallback ? this.byName.get(bareName(refName)) : undefined) ??
      [];
    if (containerNames.length > 0) {
      const containers = new Set(containerNames);
      candidates = candidates.filter(
        (candidate) =>
          candidate.containerName !== undefined &&
          containers.has(candidate.containerName),
      );
    }
    if (containerIds.length > 0) {
      for (const containerId of containerIds) {
        const scoped = candidates.filter(
          (candidate) => candidate.containerId === containerId,
        );
        if (scoped.length === 1)
          return { entry: scoped[0]!, evidence: "container_scope" };
        if (scoped.length > 1) return null;
      }
      return null;
    }
    if (candidates.length === 0) {
      return null;
    }
    const sameFile = candidates.filter((c) => c.fileId === srcFile);
    if (sameFile.length === 1) {
      return { entry: sameFile[0]!, evidence: "same_file" };
    }
    if (preferredFileIds.length > 0) {
      const preferred = new Set(preferredFileIds);
      const imported = candidates.filter((c) => preferred.has(c.fileId));
      if (imported.length === 1) {
        return { entry: imported[0]!, evidence: "preferred_file" };
      }
    }
    if (candidates.length === 1) {
      return { entry: candidates[0]!, evidence: "workspace_unique" };
    }
    // Ambiguous across files: leave unresolved (failed).
    return null;
  }

  candidates(name: string, fileIds: readonly string[]): NameEntry[] {
    const allowed = new Set(fileIds);
    return [...(this.byName.get(name) ?? [])]
      .filter((entry) => allowed.has(entry.fileId))
      .sort((a, b) => a.id.localeCompare(b.id));
  }

  snapshot(): NameEntry[] {
    return [...this.byId.values()];
  }

  load(entries: readonly NameEntry[]): void {
    this.clear();
    this.upsert(entries);
  }

  private removeId(id: string): void {
    const existing = this.byId.get(id);
    if (!existing) {
      return;
    }
    this.byId.delete(id);
    for (const key of [existing.name, bareName(existing.name)]) {
      if (!key) {
        continue;
      }
      const list = this.byName.get(key);
      if (!list) {
        continue;
      }
      const next = list.filter((e) => e.id !== id);
      if (next.length === 0) {
        this.byName.delete(key);
      } else {
        this.byName.set(key, next);
      }
    }
  }
}
