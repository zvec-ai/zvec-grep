import { isAbsolute, resolve } from "node:path";
import type {
  SourceInvalidation,
  WorkspaceSourceFreshnessProof,
} from "../engine/types.js";

export type SourceInvalidationTicket = {
  readonly version: number;
  readonly evidence: SourceInvalidation;
};

type Entry = {
  ticket: SourceInvalidationTicket;
  fingerprint: string;
  attempted: boolean;
};

/**
 * Per-path evidence ownership, independent of scheduler success or timestamps.
 * The owner records within the actual read lifetime and drains older reads
 * before acknowledgement; arbitrary late observations cannot be time-ordered.
 */
export class SourceInvalidations {
  private readonly entries = new Map<string, Entry>();
  private nextVersion = 0;

  /** Record new evidence without restarting an attempt for repeated evidence. */
  record(evidence: readonly SourceInvalidation[]): SourceInvalidationTicket[] {
    const changed = new Set<string>();
    for (const input of evidence) {
      const snapshot = copyEvidence(input);
      const path = snapshot.indexed.absolutePath;
      const fingerprint = evidenceFingerprint(snapshot);
      const previous = this.entries.get(path);
      if (previous?.fingerprint === fingerprint) continue;
      this.entries.set(path, {
        ticket: { version: ++this.nextVersion, evidence: snapshot },
        fingerprint,
        attempted: false,
      });
      changed.add(path);
    }
    return [...changed].map((path) =>
      copyTicket(this.entries.get(path)!.ticket),
    );
  }

  hasPending(): boolean {
    return this.entries.size > 0;
  }

  pending(): SourceInvalidationTicket[] {
    return this.select(() => true);
  }

  unattempted(): SourceInvalidationTicket[] {
    return this.select((entry) => !entry.attempted);
  }

  /** Claim before submitting work: the scheduler may start it synchronously. */
  claim(
    tickets: readonly SourceInvalidationTicket[],
  ): SourceInvalidationTicket[] {
    const claimed: SourceInvalidationTicket[] = [];
    for (const ticket of tickets) {
      const entry = this.currentEntry(ticket);
      if (!entry || entry.attempted) continue;
      entry.attempted = true;
      claimed.push(copyTicket(entry.ticket));
    }
    return claimed;
  }

  /** Only undo a synchronous submission failure, never a failed/aborted job. */
  releaseClaims(tickets: readonly SourceInvalidationTicket[]): void {
    for (const ticket of tickets) {
      const entry = this.currentEntry(ticket);
      if (entry) entry.attempted = false;
    }
  }

  /** Refresh only captured evidence; never replace a newer actual-read ticket. */
  recordUnverified(
    captured: readonly SourceInvalidationTicket[],
    proof: WorkspaceSourceFreshnessProof,
  ): SourceInvalidationTicket[] {
    const byPath = uniqueProofPaths(proof);
    const recorded: SourceInvalidationTicket[] = [];
    for (const ticket of captured) {
      const entry = this.currentEntry(ticket);
      if (!entry) continue;
      const path = entry.ticket.evidence.indexed.absolutePath;
      const result = byPath.get(path);
      if (result?.status !== "unverified" || !result.invalidation) continue;
      const evidence = copyEvidence(result.invalidation);
      if (
        evidence.indexed.workspaceIndexId !== proof.workspaceIndexId ||
        evidence.indexed.absolutePath !== path
      ) {
        continue;
      }
      recorded.push(...this.record([evidence]));
    }
    return recorded;
  }

  /**
   * The caller obtains this proof from the current index after repair. Workspace
   * IDs and indexedTime are not ordered: rebuilding or reverting source bytes
   * can legitimately confirm an older ticket. CAS protects newer observations.
   */
  acknowledge(
    captured: readonly SourceInvalidationTicket[],
    proof: WorkspaceSourceFreshnessProof,
  ): SourceInvalidationTicket[] {
    const byPath = uniqueProofPaths(proof);
    const acknowledged: SourceInvalidationTicket[] = [];
    for (const ticket of captured) {
      const entry = this.currentEntry(ticket);
      if (!entry) continue;
      const path = entry.ticket.evidence.indexed.absolutePath;
      const result = byPath.get(path);
      if (!result || result.status === "unverified") continue;
      if (
        result.status === "fresh" &&
        (result.indexed.workspaceIndexId !== proof.workspaceIndexId ||
          normalizedPath(result.indexed.absolutePath) !== path)
      ) {
        continue;
      }
      // A later identical drift is a new observation: source may have reverted
      // to the indexed bytes before this proof, then changed away again.
      this.entries.delete(path);
      acknowledged.push(copyTicket(entry.ticket));
    }
    return acknowledged;
  }

  private currentEntry(ticket: SourceInvalidationTicket): Entry | undefined {
    const evidence = copyEvidence(ticket.evidence);
    const entry = this.entries.get(evidence.indexed.absolutePath);
    return entry &&
      entry.ticket.version === ticket.version &&
      entry.fingerprint === evidenceFingerprint(evidence)
      ? entry
      : undefined;
  }

  private select(
    predicate: (entry: Entry) => boolean,
  ): SourceInvalidationTicket[] {
    return [...this.entries.values()]
      .filter(predicate)
      .map((entry) => copyTicket(entry.ticket));
  }
}

function uniqueProofPaths(proof: WorkspaceSourceFreshnessProof) {
  const byPath = new Map<string, (typeof proof.paths)[number] | undefined>();
  for (const result of proof.paths) {
    const path = normalizedPath(result.absolutePath);
    // Conflicting/duplicate proof entries cannot establish a unique outcome.
    byPath.set(path, byPath.has(path) ? undefined : result);
  }
  return byPath;
}

function normalizedPath(path: string): string {
  if (!isAbsolute(path)) throw new Error("Source paths must be absolute.");
  return resolve(path);
}

function copyEvidence(evidence: SourceInvalidation): SourceInvalidation {
  return {
    ...evidence,
    indexed: {
      ...evidence.indexed,
      absolutePath: normalizedPath(evidence.indexed.absolutePath),
      rootPath: normalizedPath(evidence.indexed.rootPath),
    },
  };
}

function copyTicket(
  ticket: SourceInvalidationTicket,
): SourceInvalidationTicket {
  return { version: ticket.version, evidence: copyEvidence(ticket.evidence) };
}

function evidenceFingerprint(evidence: SourceInvalidation): string {
  const { indexed } = evidence;
  return JSON.stringify([
    indexed.workspaceIndexId,
    indexed.fileId,
    indexed.absolutePath,
    indexed.rootPath,
    indexed.indexedTime,
    indexed.contentHash,
    indexed.sizeBytes,
    evidence.reason,
    evidence.observedHash,
    evidence.observedSizeBytes,
  ]);
}
