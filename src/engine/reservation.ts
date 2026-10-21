import {
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  rmdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { EngineError } from "./errors.js";
import { incompleteMarkerEntry } from "./manifest.js";
import { INCOMPLETE_RECOVERY_HINT } from "./utils/recovery-guidance.js";
import { acquireReadWriteLock, type FileLock } from "./utils/lock.js";

/**
 * Exclusive destination-ownership protocol for staging-based publication
 * (migration, import, export). Lifecycle states are explicit:
 *
 *   reserved → publishing → published-complete → committed
 *                    ↘ rollback-incomplete (blocked for operator recovery)
 *
 * - The destination is reserved at the start via its write lock; destination
 *   contents are validated under that lock; a durable INCOMPLETE marker
 *   blocks readers, discovery and writers across process death and lock
 *   cleanup until verified completion or operator recovery.
 * - Ownership (home identity, lock identity, token, staging identity) is
 *   verified before finalization, after finalization, before enumeration,
 *   before every mutation, and before any cleanup.
 * - Entry inspection never follows symlinks: any existing directory entry —
 *   including a dangling symlink — is occupied, and an inspection failure is
 *   unverifiable, never proven absence. Every published child is recorded
 *   with the entry identity it had in staging; rollback compares against
 *   that recorded identity, never adopts a post-move observation, and never
 *   overwrites content found at a staging target. Only regular files and
 *   directories are published; every staged entry's type is validated before
 *   the first move.
 * - The marker is removed only by a checked transition: access errors,
 *   unexpected absence or a foreign token fail publication instead of being
 *   converted into success.
 * - Commit is a verified successful release. On ownership loss the operation
 *   performs no further writes into the destination. When a rollback leaves
 *   recoverable state and the marker cannot be preserved or restored, the
 *   write lock is retained as the last effective block and the failure
 *   reports the required operator recovery. Foreign content is never
 *   deleted, overwritten, or merged into.
 *
 * These are best-effort fencing checks against filesystem interleavings,
 * not atomic protection against arbitrary external mutation.
 */

const INCOMPLETE_MARKER = "INCOMPLETE";

/**
 * Test-only fault-injection seam for the reservation protocol. Normal
 * callers omit it entirely. `afterChildMove` is invoked synchronously after
 * each child's move has been recorded and its destination identity verified;
 * exceptions route through ordinary rollback and failure handling, and
 * ownership is rechecked before every subsequent mutation.
 */
export type ReservationTestHooks = {
  afterChildMove?(childName: string): void;
};

export type DestinationReservation = {
  /** The reserved destination directory. */
  readonly destinationHome: string;
  /** Owned staging directory inside the reservation. */
  readonly stagingHome: string;
  /**
   * Verify ownership and staging integrity, run `finalize` (which must write
   * the manifest/metadata into the staging directory), validate every staged
   * entry's type, then move staged children into place with the manifest
   * last — rejecting any occupied or unverifiable destination name rather
   * than overwriting — remove the incomplete marker through a checked
   * transition, and commit by releasing the reservation. After a verified
   * successful release, no error path cleans the result. Any failure rolls
   * back what it provably owns, preserves blockage otherwise, and throws an
   * error that describes the state actually left behind.
   */
  publish(finalize: () => void): void;
  /**
   * Abort before commit: roll back any published children that are provably
   * owned, remove provably owned staging, clear the incomplete marker only
   * when cleanup is complete, and release the reservation through the
   * physical-ownership-checked release. Returns undefined when the
   * destination needs no further attention; otherwise a description of the
   * blockage or anomaly left behind and the required operator action.
   */
  abort(): string | undefined;
};

type DirectoryIdentity = {
  device: number;
  inode: number;
};

type EntryKind = "file" | "directory" | "symlink" | "other";

type EntryInspection =
  | { status: "present"; kind: EntryKind; identity: DirectoryIdentity }
  | { status: "absent" }
  | { status: "unverifiable"; code: string };

type PublishedChild = {
  name: string;
  /** Entry identity captured in staging before the move; rollback's
   * ownership reference, never a re-observation after the fact. */
  stagedIdentity: DirectoryIdentity;
  returned: boolean;
};

type ReservationState = {
  destinationHome: string;
  stagingHome: string;
  stagingIdentity: DirectoryIdentity | undefined;
  homeIdentity: DirectoryIdentity | undefined;
  lockDir: string;
  lockIdentity: DirectoryIdentity | undefined;
  token: string;
  lock: FileLock;
  committed: boolean;
  published: PublishedChild[];
  /** Set once failure handling ran; repeat aborts must not redo cleanup. */
  finalized: boolean;
  /** Set when the write lock is deliberately retained as the last block. */
  lockRetainedForBlockage: boolean;
  testHooks: ReservationTestHooks | undefined;
};

/** Read-only early rejection. Success is advisory; reservation repeats it under its lock. */
export function assertDestinationAvailable(
  destinationHome: string,
  markers: readonly string[] = ["manifest.json"],
): void {
  const entry = inspectEntry(destinationHome);
  if (entry.status === "absent") return;
  if (entry.status !== "present" || entry.kind !== "directory") {
    throw reservationError(
      "Destination is not a verifiable directory",
      destinationHome,
    );
  }
  const indexMarkers = markers.filter(
    (marker) => inspectEntry(join(destinationHome, marker)).status !== "absent",
  );
  if (indexMarkers.length > 0) {
    throw reservationError(
      markers.includes("format.json")
        ? "Destination already contains a transfer artifact"
        : "Destination already contains a workspace index",
      `${destinationHome} markers=${indexMarkers.join(",")}`,
    );
  }
  const markerStatus = incompleteMarkerEntry(destinationHome);
  if (markerStatus !== "absent") {
    throw reservationError(
      markerStatus === "present"
        ? "Destination contains an incomplete reserved result"
        : "Destination incomplete-marker state cannot be inspected; refusing to claim it",
      `${destinationHome} hint=${INCOMPLETE_RECOVERY_HINT}`,
    );
  }
  const unrelated = readdirSync(destinationHome).filter(
    (entry) => entry !== "locks",
  );
  if (unrelated.length > 0) {
    throw reservationError(
      "Destination contains unrelated contents and cannot be claimed",
      `${destinationHome} entries=${unrelated.join(",")}`,
    );
  }
}

export function reserveDestination(options: {
  destinationHome: string;
  operation: string;
  /** Children whose presence marks an existing index. */
  existingIndexMarkers?: readonly string[];
  /** Test-only fault-injection seam; normal callers omit it. */
  testHooks?: ReservationTestHooks;
}): DestinationReservation {
  const markers = options.existingIndexMarkers ?? ["manifest.json"];
  const lockDir = join(options.destinationHome, "locks", "home.write");
  const lock = acquireReadWriteLock(
    join(options.destinationHome, "locks", "home"),
    "write",
    { operation: options.operation },
  );

  try {
    // The early check is advisory. Repeat all destination validation under
    // the acquired lock; do not trust absence observed before copying.
    assertDestinationAvailable(options.destinationHome, markers);

    const state: ReservationState = {
      destinationHome: options.destinationHome,
      stagingHome: join(options.destinationHome, `staging-${lock.info.token}`),
      stagingIdentity: undefined,
      homeIdentity: directoryIdentity(options.destinationHome),
      lockDir,
      lockIdentity: directoryIdentity(lockDir),
      token: lock.info.token,
      lock,
      committed: false,
      published: [],
      finalized: false,
      lockRetainedForBlockage: false,
      testHooks: options.testHooks,
    };
    mkdirSync(state.stagingHome);
    state.stagingIdentity = entryIdentity(state.stagingHome);
    // The durable marker blocks readers, discovery and writers across
    // process death and lock cleanup until verified completion or the
    // documented operator recovery. "wx": the validated-empty destination
    // must not already contain one; never overwrite.
    writeFileSync(
      join(state.destinationHome, INCOMPLETE_MARKER),
      `${JSON.stringify(
        {
          token: state.token,
          operation: options.operation,
          pid: process.pid,
          startedAt: Date.now(),
        },
        null,
        2,
      )}\n`,
      { flag: "wx" },
    );

    return {
      destinationHome: state.destinationHome,
      stagingHome: state.stagingHome,
      publish(finalize) {
        publishReservation(state, finalize);
      },
      abort() {
        return abortReservation(state);
      },
    };
  } catch (error) {
    lock.release();
    throw error;
  }
}

/**
 * Append a reservation cleanup description to a thrown error, preserving its
 * code so classification and error-code checks still apply.
 */
export function appendReservationCleanup(
  error: unknown,
  cleanup: string,
): Error {
  const suffix = ` [reservation cleanup: ${cleanup}]`;
  if (error instanceof EngineError) {
    return new EngineError(`${error.message}${suffix}`, {
      code: error.code,
      context: error.context,
      cause: error,
    });
  }
  const message = error instanceof Error ? error.message : String(error);
  return new Error(`${message}${suffix}`, { cause: error });
}

function publishReservation(
  state: ReservationState,
  finalize: () => void,
): void {
  assertReservationIntact(state);
  assertStagingIntact(state);
  // Finalize first: the manifest/metadata is written into staging, then
  // moved last so readers never see a manifest without its content.
  finalize();
  // Ownership and staging integrity are verified again after finalization
  // and before every mutation; any loss stops the operation without
  // publishing.
  assertReservationIntact(state);
  assertStagingIntact(state);
  const children = readdirSync(state.stagingHome).sort((left, right) =>
    left === "manifest.json" ? 1 : right === "manifest.json" ? -1 : 0,
  );
  // Every staged entry's type and identity is validated before the first
  // move: only regular files and directories are published, and replacement
  // staging content is never adopted as owned.
  const stagedByName = new Map<
    string,
    { kind: EntryKind; identity: DirectoryIdentity }
  >();
  for (const child of children) {
    const inspection = inspectEntry(join(state.stagingHome, child));
    if (inspection.status !== "present") {
      throw failWithRollback(
        state,
        reservationError(
          `Staged child cannot be inspected before publication (${inspection.status === "unverifiable" ? inspection.code : "vanished"})`,
          `child=${child} staging=${state.stagingHome}`,
        ),
      );
    }
    if (inspection.kind !== "file" && inspection.kind !== "directory") {
      throw failWithRollback(
        state,
        reservationError(
          `Staged child has an unsupported entry type (${inspection.kind}); only regular files and directories are published`,
          `child=${child} staging=${state.stagingHome}`,
        ),
      );
    }
    stagedByName.set(child, {
      kind: inspection.kind,
      identity: inspection.identity,
    });
  }
  try {
    for (const child of children) {
      assertReservationIntact(state);
      assertStagingIntact(state);
      const staged = stagedByName.get(child)!;
      const current = inspectEntry(join(state.stagingHome, child));
      if (
        current.status !== "present" ||
        current.kind !== staged.kind ||
        !identityMatches(current.identity, staged.identity)
      ) {
        throw reservationError(
          "Staged child changed before its publication move; refusing to adopt it",
          `child=${child} staging=${state.stagingHome}`,
        );
      }
      const target = join(state.destinationHome, child);
      const targetInspection = inspectEntry(target);
      if (targetInspection.status === "present") {
        throw reservationError(
          `Destination child already exists (${targetInspection.kind}); refusing to overwrite`,
          `child=${child} destination=${state.destinationHome}`,
        );
      }
      if (targetInspection.status === "unverifiable") {
        throw reservationError(
          `Destination child cannot be inspected (${targetInspection.code}); refusing to overwrite`,
          `child=${child} destination=${state.destinationHome}`,
        );
      }
      renameSync(join(state.stagingHome, child), target);
      state.published.push({
        name: child,
        stagedIdentity: staged.identity,
        returned: false,
      });
      const moved = inspectEntry(target);
      if (
        moved.status !== "present" ||
        !identityMatches(moved.identity, staged.identity)
      ) {
        throw reservationError(
          "Moved child lost its identity at the destination",
          `child=${child} destination=${state.destinationHome}`,
        );
      }
      // Test-only seam: runs after the move is recorded and verified, inside
      // ordinary rollback/failure handling.
      state.testHooks?.afterChildMove?.(child);
    }
  } catch (error) {
    throw failWithRollback(state, error);
  }
  // Ownership and staging integrity are rechecked after the last move and
  // before the marker transition; failures enter the same finalizer.
  try {
    assertReservationIntact(state);
    assertStagingIntact(state);
  } catch (error) {
    throw failWithRollback(state, error);
  }
  // The marker transition is checked: absence, replacement or removal
  // failure can never be converted into successful publication.
  try {
    removeIncompleteMarkerChecked(state);
  } catch (error) {
    throw failWithRollback(state, error);
  }
  // Staging must be empty after the moves; a non-recursive rmdir fails
  // loudly on unexpected content instead of deleting it.
  if (!stagingIntact(state)) {
    throw failWithRollback(
      state,
      reservationError(
        "Staging directory lost its identity before cleanup",
        `staging=${state.stagingHome}`,
      ),
    );
  }
  try {
    rmdirSync(state.stagingHome);
  } catch (error) {
    throw failWithRollback(
      state,
      reservationError(
        `Staging directory could not be removed after publication (${errorCode(
          error,
        )}); unexpected content is preserved`,
        `staging=${state.stagingHome}`,
      ),
    );
  }

  // The commit point is a verified successful release. A failed release is
  // ownership loss: no further writes into the destination, and the
  // operation reports failure for operator review.
  const released = state.lock.release();
  if (!released) {
    state.finalized = true;
    throw reservationError(
      "Reservation ownership was lost at commit; the destination was left untouched after marker removal and is governed by the conflicting lock state; operator review required",
      `destination=${state.destinationHome}`,
    );
  }
  state.committed = true;
}

/**
 * Failure finalizer for publication: roll back provably owned children, then
 * preserve the remaining blockage and describe the state left behind. Never
 * performs a write after ownership loss, and never releases the last
 * effective block while recoverable state remains.
 */
function failWithRollback(
  state: ReservationState,
  cause: unknown,
): EngineError {
  state.finalized = true;
  const causeMessage =
    cause instanceof EngineError
      ? cause.message
      : cause instanceof Error
        ? cause.message
        : String(cause);
  const rollbackComplete = rollbackPublishedChildren(state);
  const { marker, released } = settleBlockage(state);
  const rollbackDescription = rollbackComplete
    ? "publication was rolled back completely, with its payload returned to staging"
    : "rollback is incomplete: published payload remains at the destination";
  return reservationError(
    `${causeMessage}; ${rollbackDescription}; ${markerBlockageDescription(marker, released)}; recover after all writers are quiescent`,
    `destination=${state.destinationHome}`,
  );
}

/**
 * Move published children back into staging, one ownership check at a time.
 * Compares each destination child against the entry identity recorded in
 * staging before the move; stops at the first mismatch, refusal or I/O
 * failure and leaves everything else untouched. Never overwrites content
 * found at a staging target — any entry there is occupied — and treats
 * inspection failures as unverifiable, never as absence. A child already
 * back at its staging target with the recorded identity and absent from the
 * destination counts as an earlier verified return.
 */
function rollbackPublishedChildren(state: ReservationState): boolean {
  for (const record of [...state.published].reverse()) {
    if (record.returned) {
      continue;
    }
    if (!reservationIntact(state)) {
      return false;
    }
    if (!stagingIntact(state)) {
      return false;
    }
    const destinationChild = join(state.destinationHome, record.name);
    const stagingChild = join(state.stagingHome, record.name);
    const destination = inspectEntry(destinationChild);
    if (
      destination.status !== "present" ||
      !identityMatches(destination.identity, record.stagedIdentity)
    ) {
      // Replaced, removed, or unverifiable at the destination: if the
      // original already sits at its staging target with the recorded
      // identity, an earlier verified return completed; otherwise the
      // foreign or unknown state is preserved.
      if (
        destination.status === "absent" &&
        identityMatches(entryIdentity(stagingChild), record.stagedIdentity)
      ) {
        record.returned = true;
        continue;
      }
      return false;
    }
    // The destination child is provably ours; its staging target must be
    // empty. Any entry there is occupied and never overwritten.
    if (inspectEntry(stagingChild).status !== "absent") {
      return false;
    }
    try {
      renameSync(destinationChild, stagingChild);
    } catch {
      return false;
    }
    if (!identityMatches(entryIdentity(stagingChild), record.stagedIdentity)) {
      return false;
    }
    record.returned = true;
  }
  return state.published.every((record) => record.returned);
}

/**
 * Ensure the durable blockage survives a failed operation. Returns "present"
 * when a marker with this reservation's token (or an entry confirmed present
 * by lstat but unreadable — such an entry really does supply the durable
 * block and is never overwritten) is in place, including after a successful
 * restore; "foreign" when a replaced marker with another token stands
 * (preserved, never touched); "unknown" when inspection fails — an
 * inspection error does not establish that an entry exists, so uncertainty
 * is never treated as confirmed blockage; "absent" when no enforceable
 * blockage exists. Restore is attempted only while reservation ownership is
 * verifiable, and nothing is written after ownership loss.
 */
function ensureMarkerPreserved(
  state: ReservationState,
): "present" | "foreign" | "absent" | "unknown" {
  const marker = join(state.destinationHome, INCOMPLETE_MARKER);
  const status = incompleteMarkerEntry(state.destinationHome);
  if (status === "present") {
    try {
      const current = JSON.parse(readFileSync(marker, "utf8")) as {
        token?: string;
      };
      return current?.token === state.token ? "present" : "foreign";
    } catch {
      return "present";
    }
  }
  if (status === "unverifiable") {
    return "unknown";
  }
  if (!reservationIntact(state)) {
    return "absent";
  }
  try {
    // "wx": never follow a replaced marker path or overwrite anything.
    writeFileSync(
      marker,
      `${JSON.stringify(
        {
          token: state.token,
          pid: process.pid,
          startedAt: Date.now(),
          operation: "rollback-block",
        },
        null,
        2,
      )}\n`,
      { flag: "wx" },
    );
    return "present";
  } catch {
    return "absent";
  }
}

type MarkerOutcome = "present" | "foreign" | "absent" | "unknown";

/**
 * Settle the remaining blockage after a failure, shared by publication
 * rollback and abort so the decision cannot diverge: release the lock only
 * when a durable marker is confirmed present (ours, or a preserved foreign
 * one); otherwise — marker absent and unrestorable, or its state
 * unverifiable — retain the owned write lock as the last effective block.
 */
function settleBlockage(state: ReservationState): {
  marker: MarkerOutcome;
  released: boolean;
} {
  const marker = ensureMarkerPreserved(state);
  if (marker === "present" || marker === "foreign") {
    return { marker, released: state.lock.release() };
  }
  state.lockRetainedForBlockage = true;
  return { marker, released: false };
}

function markerBlockageDescription(
  marker: MarkerOutcome,
  released: boolean,
): string {
  if (marker === "present" || marker === "foreign") {
    return `the destination remains blocked by ${marker === "present" ? "the INCOMPLETE marker" : "a foreign INCOMPLETE marker preserved for operator review"}${released ? "" : "; the lock release reported ownership loss"}`;
  }
  return `${marker === "unknown" ? "the INCOMPLETE marker state could not be inspected" : "no INCOMPLETE marker could be preserved or restored"}; the write lock is retained as the last block`;
}

/**
 * The publication marker transition. Any anomaly — absence, unreadable
 * content, a foreign token, or a removal failure — throws instead of being
 * converted into successful publication.
 */
function removeIncompleteMarkerChecked(state: ReservationState): void {
  const marker = join(state.destinationHome, INCOMPLETE_MARKER);
  const status = incompleteMarkerEntry(state.destinationHome);
  if (status === "absent") {
    throw reservationError(
      "Incomplete marker is unexpectedly absent at publication; refusing to report success over external interference",
      `destination=${state.destinationHome}`,
    );
  }
  if (status === "unverifiable") {
    throw reservationError(
      "Incomplete marker cannot be inspected at publication; refusing to report success over uncertainty",
      `destination=${state.destinationHome}`,
    );
  }
  let current: { token?: string };
  try {
    current = JSON.parse(readFileSync(marker, "utf8")) as { token?: string };
  } catch (error) {
    throw reservationError(
      `Incomplete marker is unreadable at publication (${errorCode(error)})`,
      `destination=${state.destinationHome}`,
    );
  }
  if (current?.token !== state.token) {
    throw reservationError(
      "Incomplete marker was replaced by foreign state; refusing to publish over it",
      `destination=${state.destinationHome}`,
    );
  }
  try {
    rmSync(marker, { force: true });
  } catch (error) {
    throw reservationError(
      `Failed to remove the incomplete marker (${errorCode(error)})`,
      `destination=${state.destinationHome}`,
    );
  }
}

function abortReservation(state: ReservationState): string | undefined {
  if (state.committed) {
    return undefined;
  }
  if (state.finalized || state.lockRetainedForBlockage) {
    // Failure handling already ran (or deliberately retained the lock); its
    // error describes the state left behind.
    return undefined;
  }
  try {
    if (!reservationIntact(state)) {
      // Ownership lost: no writes into the destination at all. The release
      // is ownership-checked and will not delete a replacement's lock.
      state.lock.release();
      return "aborted without cleanup: reservation ownership was lost; the destination was left untouched for operator review";
    }
    if (state.published.length > 0) {
      const rollbackComplete = rollbackPublishedChildren(state);
      if (!rollbackComplete) {
        state.finalized = true;
        const { marker, released } = settleBlockage(state);
        return `abort left partial published payload in place; ${markerBlockageDescription(marker, released)}; recover after all writers are quiescent`;
      }
    }
    // Nothing remains published: staging removal is a checked transition.
    // The marker is cleared only after owned staging removal is confirmed;
    // failed, skipped (foreign replacement, never deleted), unverifiable or
    // unexplained-absent cleanup preserves the marker, or retains the owned
    // lock when no durable marker can be established.
    let stagingNote = "";
    let stagingCleared = false;
    const stagingPresence = inspectEntry(state.stagingHome);
    if (stagingPresence.status === "absent") {
      // Unexplained disappearance is not completed cleanup: recoverable
      // payload may remain elsewhere in the destination.
      stagingNote =
        "the staging directory is unexpectedly absent (its payload may remain elsewhere in the destination; operator review required)";
    } else if (stagingPresence.status === "unverifiable") {
      stagingNote = `the staging directory could not be inspected (${stagingPresence.code})`;
    } else if (
      stagingPresence.kind === "directory" &&
      identityMatches(stagingPresence.identity, state.stagingIdentity)
    ) {
      try {
        rmSync(state.stagingHome, { recursive: true, force: true });
        stagingCleared = true;
      } catch (error) {
        stagingNote = `owned staging could not be removed (${errorCode(error)}); its payload remains`;
      }
    } else {
      // A replacement — including a symlink alias to the original contents —
      // is preserved, never deleted through.
      stagingNote =
        stagingPresence.kind === "directory"
          ? "staging was replaced by foreign content, which was preserved in place"
          : `staging was replaced by a ${stagingPresence.kind} entry, which was preserved in place`;
    }
    if (!stagingCleared) {
      state.finalized = true;
      const { marker, released } = settleBlockage(state);
      return `abort cleanup incomplete: ${stagingNote}; ${markerBlockageDescription(marker, released)}; recover after all writers are quiescent`;
    }
    const markerNote = clearMarkerAfterCleanup(state);
    const released = state.lock.release();
    const notes = [stagingNote, markerNote];
    if (!released) {
      notes.push("the lock release reported ownership loss");
    }
    return notes.filter((note) => note.length > 0).length > 0
      ? `abort completed with reservations: ${notes.join("; ")}`
      : undefined;
  } finally {
    state.finalized = true;
  }
}

/**
 * Marker removal during a complete cleanup. Returns a description of any
 * anomaly; the marker is only ever removed when it carries this
 * reservation's token.
 */
function clearMarkerAfterCleanup(state: ReservationState): string {
  const marker = join(state.destinationHome, INCOMPLETE_MARKER);
  const status = incompleteMarkerEntry(state.destinationHome);
  if (status === "absent") {
    return "the INCOMPLETE marker was already absent (external interference; operator review required)";
  }
  if (status === "unverifiable") {
    return "the INCOMPLETE marker state could not be inspected; owned contents were removed, and any marker entry that does remain keeps blocking the destination";
  }
  let current: { token?: string };
  try {
    current = JSON.parse(readFileSync(marker, "utf8")) as { token?: string };
  } catch (error) {
    return `the INCOMPLETE marker is unreadable (${errorCode(error)}) and was left in place as blockage`;
  }
  if (current?.token !== state.token) {
    return "the INCOMPLETE marker was replaced by foreign state and was preserved; operator review required";
  }
  try {
    rmSync(marker, { force: true });
    return "";
  } catch (error) {
    return `the INCOMPLETE marker could not be removed (${errorCode(error)}); the destination remains blocked and requires operator recovery`;
  }
}

function assertStagingIntact(state: ReservationState): void {
  if (!stagingIntact(state)) {
    throw reservationError(
      "Staging directory ownership was lost or replaced; refusing to publish from it",
      `staging=${state.stagingHome}`,
    );
  }
}

/**
 * Non-following staging validation: the staging path must still be the
 * recorded directory entry itself. A symlink alias to the original contents
 * is a replacement entry, never the owned staging — and deletion through an
 * alias would delete the link, not the payload.
 */
function stagingIntact(state: ReservationState): boolean {
  const inspection = inspectEntry(state.stagingHome);
  return (
    inspection.status === "present" &&
    inspection.kind === "directory" &&
    identityMatches(inspection.identity, state.stagingIdentity)
  );
}

function reservationIntact(state: ReservationState): boolean {
  if (
    !identityMatches(
      directoryIdentity(state.destinationHome),
      state.homeIdentity,
    )
  ) {
    return false;
  }
  if (!identityMatches(directoryIdentity(state.lockDir), state.lockIdentity)) {
    return false;
  }
  const info = readLockInfoSafe(state.lockDir);
  return info?.token === state.token;
}

function assertReservationIntact(state: ReservationState): void {
  if (!reservationIntact(state)) {
    throw reservationError(
      "Destination reservation ownership was lost; aborting without touching the destination",
      `destination=${state.destinationHome}`,
    );
  }
}

function identityMatches(
  current: DirectoryIdentity | undefined,
  expected: DirectoryIdentity | undefined,
): boolean {
  return (
    current !== undefined &&
    expected !== undefined &&
    current.device === expected.device &&
    current.inode === expected.inode
  );
}

function directoryIdentity(path: string): DirectoryIdentity | undefined {
  try {
    const info = statSync(path);
    return { device: info.dev, inode: info.ino };
  } catch {
    return undefined;
  }
}

/**
 * Non-following entry inspection: any existing directory entry — including
 * a dangling symlink — is "present" with its own identity; only
 * ENOENT/ENOTDIR is "absent"; anything else is "unverifiable", never proven
 * absence.
 */
function inspectEntry(path: string): EntryInspection {
  try {
    const info = lstatSync(path);
    return {
      status: "present",
      kind: info.isFile()
        ? "file"
        : info.isDirectory()
          ? "directory"
          : info.isSymbolicLink()
            ? "symlink"
            : "other",
      identity: { device: info.dev, inode: info.ino },
    };
  } catch (error) {
    if (isAbsence(error)) {
      return { status: "absent" };
    }
    return { status: "unverifiable", code: errorCode(error) };
  }
}

function entryIdentity(path: string): DirectoryIdentity | undefined {
  const inspection = inspectEntry(path);
  return inspection.status === "present" ? inspection.identity : undefined;
}

function readLockInfoSafe(lockDir: string): { token?: string } | null {
  try {
    return JSON.parse(readFileSync(join(lockDir, "lock.json"), "utf8")) as {
      token?: string;
    };
  } catch {
    return null;
  }
}

function isAbsence(error: unknown): boolean {
  const code = errorCode(error);
  return code === "ENOENT" || code === "ENOTDIR";
}

function errorCode(error: unknown): string {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : "unknown";
}

function reservationError(message: string, context: string): EngineError {
  return new EngineError(message, {
    code: "ZVEC_GREP.ENGINE.RESERVATION.FAILED",
    context,
  });
}
