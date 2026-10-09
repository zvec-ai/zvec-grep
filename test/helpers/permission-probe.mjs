import { chmodSync, existsSync } from "node:fs";

// Tri-state permission-capability probe for capability-branched fixtures.
//
// A probe distinguishes three outcomes:
//   "denied"  — the platform enforced the expected denial (EACCES/EPERM);
//   "allowed" — the operation genuinely succeeded (advisory mode bits);
// and any other error is unexpected and propagates: misclassifying it as
// advisory would silently drop the strict failure-path coverage.
//
// Every probe emits a test diagnostic recording the operation, platform,
// privilege context, probe outcome and the branch the fixture selected,
// so an unexercised strict branch is observable in test output.
//
// In required non-root Linux/macOS environments, mode bits must be able to
// deny the owner: an inability to induce the denial is a fixture or
// precondition failure, not a silent pass of reduced coverage. Windows and
// privileged (root) contexts follow the measured capability instead.

export const DENIED_ERROR_CODES = new Set(["EACCES", "EPERM"]);

export function isPosixNonRoot() {
  return (
    (process.platform === "linux" || process.platform === "darwin") &&
    typeof process.getuid === "function" &&
    process.getuid() !== 0
  );
}

export function privilegeContext() {
  return `platform=${process.platform} uid=${
    typeof process.getuid === "function" ? process.getuid() : "n/a"
  }`;
}

/**
 * Run one denial probe. `operation` names it for diagnostics; `attempt`
 * performs the filesystem operation once and must not swallow errors.
 * Returns "denied" or "allowed" and rethrows unexpected errors.
 */
export async function probeDenial(t, operation, attempt, source = "real") {
  let outcome;
  let probeError;
  try {
    await attempt();
    outcome = "allowed";
  } catch (error) {
    if (DENIED_ERROR_CODES.has(error.code)) {
      outcome = "denied";
      probeError = `${error.code}`;
    } else {
      t.diagnostic(
        `permission-probe operation=${operation} source=${source} ${privilegeContext()} probe=unexpected-error code=${error.code}`,
      );
      throw error;
    }
  }
  t.diagnostic(
    `permission-probe operation=${operation} source=${source} ${privilegeContext()} probe=${outcome}` +
      (probeError ? ` error=${probeError}` : ""),
  );
  return outcome;
}

/**
 * Guard the strict branch in environments where mode bits must deny:
 * fails the fixture when the denial could not be induced there.
 */
export function assertDenialInducible(t, operation, outcome) {
  if (outcome === "allowed" && isPosixNonRoot()) {
    throw new Error(
      `fixture precondition failure: ${operation} could not induce the expected denial in a required non-root ${process.platform} environment (${privilegeContext()}); strict failure-path coverage was not exercised`,
    );
  }
}

/** Synchronous denial probe for use inside synchronous test hooks. */
export function probeDenialSync(t, operation, attempt, source = "real") {
  let outcome;
  let probeError;
  try {
    attempt();
    outcome = "allowed";
  } catch (error) {
    if (DENIED_ERROR_CODES.has(error.code)) {
      outcome = "denied";
      probeError = `${error.code}`;
    } else {
      t.diagnostic(
        `permission-probe operation=${operation} source=${source} ${privilegeContext()} probe=unexpected-error code=${error.code}`,
      );
      throw error;
    }
  }
  t.diagnostic(
    `permission-probe operation=${operation} source=${source} ${privilegeContext()} probe=${outcome}` +
      (probeError ? ` error=${probeError}` : ""),
  );
  return outcome;
}

/**
 * Guarded synchronous probe for test hooks: tri-state classification,
 * selected-branch diagnostic and the required-denial guard in one call.
 * Unexpected probe errors propagate as probe failures; the caller must
 * surface them as such instead of letting publication error handling
 * reinterpret them.
 */
export function guardedSyncPermissionProbe(
  t,
  operation,
  attempt,
  source = "real",
) {
  const outcome = probeDenialSync(t, operation, attempt, source);
  t.diagnostic(
    `permission-branch operation=${operation} branch=${
      outcome === "denied" ? "strict" : "advisory"
    }`,
  );
  assertDenialInducible(t, operation, outcome);
  return outcome;
}

/**
 * Failure-safe permission restoration: registers test teardown that runs
 * after probe, guard or assertion failures, and returns the restore
 * function so controls can verify the restoration itself. The target's
 * permissions are only touched when it still exists.
 */
export function registerPermissionRestore(t, target, mode = 0o755) {
  const restore = () => {
    if (existsSync(target)) {
      chmodSync(target, mode);
    }
  };
  t.after(restore);
  return restore;
}
