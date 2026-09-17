import assert from "node:assert/strict";
import test from "node:test";
import { SourceInvalidations } from "../../dist/daemon/source-invalidations.js";

test("source invalidations normalize paths and deduplicate repeated evidence", () => {
  const tracker = new SourceInvalidations();
  assert.equal(tracker.hasPending(), false);
  assert.deepEqual(tracker.pending(), []);
  assert.deepEqual(tracker.unattempted(), []);
  const input = evidence("/repo/src/../pool.ts");
  input.indexed.rootPath = "/repo/./";
  const [ticket] = tracker.record([input, evidence()]);
  assert.equal(ticket.version, 1);
  assert.equal(ticket.evidence.indexed.absolutePath, "/repo/pool.ts");
  assert.equal(ticket.evidence.indexed.rootPath, "/repo");
  assert.equal(tracker.hasPending(), true);
  assert.deepEqual(tracker.record([evidence()]), []);
  assert.deepEqual(tracker.pending(), [ticket]);
  assert.deepEqual(tracker.unattempted(), [ticket]);
  assert.equal(input.indexed.absolutePath, "/repo/src/../pool.ts");
});

test("source invalidation versions follow changed evidence, not clock order", () => {
  const tracker = new SourceInvalidations();
  const input = evidence();
  const [first] = tracker.record([input]);
  const changes = [
    { indexed: { indexedTime: 1 } },
    { indexed: { contentHash: "another-indexed-hash" } },
    { indexed: { workspaceIndexId: "rebuilt-workspace" } },
    { indexed: { fileId: "replacement-file" } },
    { indexed: { rootPath: "/repo/src" } },
    { indexed: { sizeBytes: 45 } },
    { reason: "missing" },
    { observedHash: "changed-again" },
    { observedSizeBytes: 45 },
  ];
  let version = first.version;
  for (const change of changes) {
    const next = {
      ...input,
      ...change,
      indexed: { ...input.indexed, ...change.indexed },
    };
    const [ticket] = tracker.record([next]);
    assert.ok(ticket.version > version);
    version = ticket.version;
    assert.deepEqual(tracker.record([next]), []);
  }
  assert.equal(tracker.pending().length, 1);
});

test("record returns only the current ticket when one batch revises a path twice", () => {
  const tracker = new SourceInvalidations();
  const revised = { ...evidence(), observedHash: "newest" };
  const recorded = tracker.record([evidence(), revised]);
  assert.equal(recorded.length, 1);
  assert.equal(recorded[0].version, 2);
  assert.deepEqual(recorded, tracker.pending());
});

test("claim is CAS and repeated queries cannot restart already attempted evidence", () => {
  const tracker = new SourceInvalidations();
  const tickets = tracker.record([evidence(), evidence("/repo/other.ts")]);
  assert.deepEqual(tracker.claim([tickets[0], tickets[0]]), [tickets[0]]);
  assert.deepEqual(tracker.claim([tickets[0]]), []);
  assert.deepEqual(tracker.record([evidence()]), []);
  assert.deepEqual(tracker.unattempted(), [tickets[1]]);
  assert.deepEqual(tracker.pending(), tickets);
  const [newer] = tracker.record([{ ...evidence(), observedHash: "newer" }]);
  assert.deepEqual(tracker.claim([tickets[0]]), []);
  assert.deepEqual(tracker.claim([newer]), [newer]);
});

test("releaseClaims rolls back submission failure without releasing newer claims", () => {
  const tracker = new SourceInvalidations();
  const [old] = tracker.record([evidence()]);
  tracker.claim([old]);
  tracker.releaseClaims([old]);
  assert.deepEqual(tracker.unattempted(), [old]);
  tracker.claim([old]);
  const [newer] = tracker.record([{ ...evidence(), observedHash: "newer" }]);
  tracker.claim([newer]);
  tracker.releaseClaims([old]);
  assert.deepEqual(tracker.unattempted(), []);
  assert.deepEqual(tracker.pending(), [newer]);
});

test("ticket evidence is part of CAS, not only its numeric version", () => {
  const tracker = new SourceInvalidations();
  const [ticket] = tracker.record([evidence()]);
  const tampered = structuredClone(ticket);
  tampered.evidence.observedHash = "forged";
  assert.deepEqual(tracker.claim([tampered]), []);
  tracker.claim([ticket]);
  tracker.releaseClaims([tampered]);
  assert.deepEqual(tracker.unattempted(), []);
  assert.deepEqual(tracker.acknowledge([tampered], freshProof()), []);
  assert.deepEqual(tracker.pending(), [ticket]);
});

test("fresh and absent proofs acknowledge only their captured paths", () => {
  const tracker = new SourceInvalidations();
  const tickets = tracker.record([
    evidence(),
    evidence("/repo/deleted.ts"),
    evidence("/repo/unproven.ts"),
  ]);
  tracker.claim(tickets);
  const proof = freshProof();
  proof.paths.push({ absolutePath: "/repo/deleted.ts", status: "absent" });
  assert.deepEqual(tracker.acknowledge(tickets, proof), tickets.slice(0, 2));
  assert.deepEqual(tracker.pending(), [tickets[2]]);
  assert.equal(tracker.hasPending(), true);
  assert.deepEqual(tracker.unattempted(), []);
  assert.deepEqual(tracker.acknowledge(tickets, proof), []);
});

test("old captured tickets and their proofs cannot acknowledge newer evidence", () => {
  const tracker = new SourceInvalidations();
  const old = tracker.record([evidence()]);
  tracker.claim(old);
  const newer = tracker.record([{ ...evidence(), observedHash: "newer" }]);
  for (const proof of [
    freshProof(),
    {
      workspaceIndexId: "workspace",
      paths: [{ absolutePath: "/repo/pool.ts", status: "absent" }],
    },
  ]) {
    assert.deepEqual(tracker.acknowledge(old, proof), []);
    assert.deepEqual(tracker.pending(), newer);
  }
});

test("captured old-workspace evidence cannot clear a version recorded after rebuild", () => {
  const tracker = new SourceInvalidations();
  const old = tracker.record([evidence()]);
  const rebuilt = evidence();
  rebuilt.indexed.workspaceIndexId = "rebuilt-workspace";
  rebuilt.indexed.indexedTime = 1;
  const current = tracker.record([rebuilt]);
  assert.deepEqual(tracker.acknowledge(old, freshProof()), []);
  assert.deepEqual(tracker.pending(), current);
  assert.deepEqual(tracker.unattempted(), current);
});

test("unverified repair proof records a changed source version for another bounded attempt", () => {
  const tracker = new SourceInvalidations();
  const captured = tracker.record([evidence()]);
  tracker.claim(captured);
  const nextEvidence = evidence();
  nextEvidence.indexed.contentHash = "observed-hash";
  nextEvidence.indexed.indexedTime = 1;
  nextEvidence.observedHash = "changed-during-embedding";
  const proof = unverifiedProof(nextEvidence);
  const recorded = tracker.recordUnverified(captured, proof);
  assert.equal(recorded.length, 1);
  assert.ok(recorded[0].version > captured[0].version);
  assert.deepEqual(recorded[0].evidence, nextEvidence);
  assert.deepEqual(tracker.unattempted(), recorded);
  assert.deepEqual(tracker.acknowledge(captured, proof), []);
  nextEvidence.indexed.contentHash = "mutated-input";
  proof.paths[0].invalidation.observedHash = "mutated-proof";
  assert.equal(
    tracker.pending()[0].evidence.indexed.contentHash,
    "observed-hash",
  );
  assert.equal(
    tracker.pending()[0].evidence.observedHash,
    "changed-during-embedding",
  );
});

test("identical unverified repair proof does not restart an attempted version", () => {
  const tracker = new SourceInvalidations();
  const captured = tracker.record([evidence()]);
  tracker.claim(captured);
  const proof = unverifiedProof(evidence());
  assert.deepEqual(tracker.recordUnverified(captured, proof), []);
  assert.deepEqual(tracker.recordUnverified(captured, proof), []);
  assert.deepEqual(tracker.pending(), captured);
  assert.deepEqual(tracker.unattempted(), []);
});

test("stale captured repair proof cannot overwrite newer actual-read evidence", () => {
  const tracker = new SourceInvalidations();
  const captured = tracker.record([evidence()]);
  tracker.claim(captured);
  const actualRead = { ...evidence(), observedHash: "newest-actual-read" };
  const current = tracker.record([actualRead]);
  tracker.claim(current);
  const olderProof = unverifiedProof({
    ...evidence(),
    observedHash: "older-proof",
  });
  assert.deepEqual(tracker.recordUnverified(captured, olderProof), []);
  assert.deepEqual(tracker.pending(), current);
  assert.deepEqual(tracker.unattempted(), []);
});

test("unverified proof refresh requires a unique matching path and workspace identity", () => {
  const tracker = new SourceInvalidations();
  const captured = tracker.record([evidence()]);
  tracker.claim(captured);
  const changed = { ...evidence(), observedHash: "changed" };
  const proofs = [
    freshProof(),
    {
      workspaceIndexId: "workspace",
      paths: [{ absolutePath: "/repo/pool.ts", status: "absent" }],
    },
    {
      workspaceIndexId: "workspace",
      paths: [
        {
          absolutePath: "/repo/pool.ts",
          status: "unverified",
          reason: "not_indexed",
        },
      ],
    },
    unverifiedProof(evidence("/repo/unrelated.ts")),
  ];
  const wrongWorkspace = unverifiedProof(changed);
  wrongWorkspace.workspaceIndexId = "different-workspace";
  proofs.push(wrongWorkspace);
  const wrongPath = unverifiedProof(changed);
  wrongPath.paths[0].absolutePath = "/repo/other.ts";
  proofs.push(wrongPath);
  const duplicate = unverifiedProof(changed);
  duplicate.paths.push({ ...duplicate.paths[0] });
  proofs.push(duplicate);
  for (const proof of proofs) {
    assert.deepEqual(tracker.recordUnverified(captured, proof), []);
    assert.deepEqual(tracker.pending(), captured);
    assert.deepEqual(tracker.unattempted(), []);
  }
  const rebuilt = structuredClone(changed);
  rebuilt.indexed.workspaceIndexId = "rebuilt-workspace";
  const [ticket] = tracker.recordUnverified(captured, unverifiedProof(rebuilt));
  assert.equal(ticket.evidence.indexed.workspaceIndexId, "rebuilt-workspace");
});

test("a rebuilt workspace may confirm an unchanged captured ticket", () => {
  for (const status of ["fresh", "absent"]) {
    const tracker = new SourceInvalidations();
    const tickets = tracker.record([evidence()]);
    const proof = freshProof();
    proof.workspaceIndexId = "rebuilt-workspace";
    if (status === "fresh") {
      proof.paths[0].indexed.workspaceIndexId = "rebuilt-workspace";
      proof.paths[0].indexed.fileId = "rebuilt-file";
      proof.paths[0].indexed.indexedTime = 1;
    } else {
      proof.paths = [{ absolutePath: "/repo/pool.ts", status: "absent" }];
    }
    assert.deepEqual(tracker.acknowledge(tickets, proof), tickets);
    assert.equal(tracker.hasPending(), false);
  }
});

test("fresh proof can confirm source reverted to its original indexed bytes", () => {
  const tracker = new SourceInvalidations();
  const tickets = tracker.record([evidence()]);
  const proof = freshProof();
  proof.paths[0].indexed = structuredClone(tickets[0].evidence.indexed);
  assert.deepEqual(tracker.acknowledge(tickets, proof), tickets);
  assert.deepEqual(tracker.pending(), []);
});

test("unverified, unrelated and internally inconsistent proofs retain evidence", () => {
  const tracker = new SourceInvalidations();
  const tickets = tracker.record([evidence()]);
  const proofs = [
    { workspaceIndexId: "workspace", paths: [] },
    freshProof("/repo/unrelated.ts"),
    {
      workspaceIndexId: "workspace",
      paths: [
        {
          absolutePath: "/repo/pool.ts",
          status: "unverified",
          reason: "unreadable",
        },
      ],
    },
  ];
  const wrongWorkspace = freshProof();
  wrongWorkspace.paths[0].indexed.workspaceIndexId = "old-workspace";
  proofs.push(wrongWorkspace);
  const wrongPath = freshProof();
  wrongPath.paths[0].indexed.absolutePath = "/repo/unrelated.ts";
  proofs.push(wrongPath);
  const duplicate = freshProof();
  duplicate.paths.push({ absolutePath: "/repo/pool.ts", status: "absent" });
  proofs.push(duplicate);
  for (const proof of proofs) {
    assert.deepEqual(tracker.acknowledge(tickets, proof), []);
    assert.deepEqual(tracker.pending(), tickets);
  }
});

test("an identical drift after a proven source revert creates a new repair ticket", () => {
  const tracker = new SourceInvalidations();
  const tickets = tracker.record([evidence()]);
  tracker.claim(tickets);
  const reverted = freshProof();
  reverted.paths[0].indexed = structuredClone(tickets[0].evidence.indexed);
  tracker.acknowledge(tickets, reverted);
  assert.equal(tracker.hasPending(), false);
  assert.deepEqual(tracker.pending(), []);
  assert.deepEqual(tracker.unattempted(), []);
  assert.deepEqual(tracker.claim(tickets), []);
  tracker.releaseClaims(tickets);
  assert.deepEqual(tracker.unattempted(), []);
  const [next] = tracker.record([evidence()]);
  assert.ok(next.version > tickets[0].version);
  assert.deepEqual(next.evidence, tickets[0].evidence);
  assert.equal(tracker.hasPending(), true);
  assert.deepEqual(tracker.unattempted(), [next]);
  assert.deepEqual(tracker.acknowledge(tickets, reverted), []);
  assert.deepEqual(tracker.claim([next]), [next]);
});

test("inputs and returned tickets cannot mutate stored evidence or claims", () => {
  const tracker = new SourceInvalidations();
  const input = evidence();
  const recorded = tracker.record([input]);
  const expected = structuredClone(recorded);
  input.indexed.contentHash = "mutated-input";
  recorded[0].evidence.indexed.contentHash = "mutated-output";
  tracker.pending()[0].evidence.observedHash = "mutated-pending";
  tracker.unattempted()[0].evidence.reason = "missing";
  assert.deepEqual(tracker.pending(), expected);
  const claimed = tracker.claim(expected);
  claimed[0].evidence.indexed.fileId = "mutated-claim";
  assert.deepEqual(tracker.pending(), expected);
  assert.deepEqual(tracker.unattempted(), []);
  const acknowledged = tracker.acknowledge(expected, freshProof());
  acknowledged[0].evidence.reason = "missing";
  assert.equal(tracker.hasPending(), false);
  const [next] = tracker.record([evidence()]);
  assert.deepEqual(next.evidence, expected[0].evidence);
});

test("proof paths normalize without interpreting filesystem aliases", () => {
  const tracker = new SourceInvalidations();
  const tickets = tracker.record([evidence()]);
  const proof = freshProof("/repo/src/../pool.ts");
  assert.deepEqual(tracker.acknowledge(tickets, proof), tickets);
  assert.equal(proof.paths[0].absolutePath, "/repo/src/../pool.ts");
});

test("source evidence requires absolute file and configured-root paths", () => {
  const tracker = new SourceInvalidations();
  assert.throws(() => tracker.record([evidence("relative.ts")]), /absolute/);
  const input = evidence();
  input.indexed.rootPath = "relative-root";
  assert.throws(() => tracker.record([input]), /absolute/);
  assert.equal(tracker.hasPending(), false);
});

function evidence(path = "/repo/pool.ts") {
  return {
    indexed: {
      workspaceIndexId: "workspace",
      fileId: `file:${path}`.replace("src/../", ""),
      absolutePath: path,
      rootPath: "/repo",
      indexedTime: 100,
      contentHash: "indexed-hash",
      sizeBytes: 44,
    },
    reason: "hash_mismatch",
    observedHash: "observed-hash",
    observedSizeBytes: 44,
  };
}

function freshProof(path = "/repo/pool.ts") {
  const indexed = evidence(path).indexed;
  return {
    workspaceIndexId: "workspace",
    paths: [
      {
        absolutePath: path,
        status: "fresh",
        indexed: { ...indexed, contentHash: "observed-hash", indexedTime: 101 },
      },
    ],
  };
}

function unverifiedProof(invalidation) {
  return {
    workspaceIndexId: invalidation.indexed.workspaceIndexId,
    paths: [
      {
        absolutePath: invalidation.indexed.absolutePath,
        status: "unverified",
        reason: invalidation.reason,
        invalidation,
      },
    ],
  };
}
