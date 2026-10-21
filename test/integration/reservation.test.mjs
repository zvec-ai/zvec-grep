import assert from "node:assert/strict";
import {
  chmodSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readlinkSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { ZVecInitialize, ZVecLogLevel } from "@zvec/zvec";
import { migrateWorkspaceIndex } from "../../dist/engine/migrate/index.js";
import { reserveDestination } from "../../dist/engine/reservation.js";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../../dist/engine/transfer/index.js";
import { readWorkspaceManifest } from "../../dist/engine/manifest.js";
import { assertNoWriteLock } from "../../dist/engine/utils/lock.js";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
import {
  assertDenialInducible,
  guardedSyncPermissionProbe,
  isPosixNonRoot,
  probeDenial,
} from "../helpers/permission-probe.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";

useIsolatedZvecGrepHome();

const FIXTURES = {
  "docs/guide.md": "# Guide\n\nReservation protocol fixture content.\n",
};

async function makeLegacySource(t, parent) {
  const sourceRoot = join(parent, "original");
  await mkdir(join(sourceRoot, "docs"), { recursive: true });
  await writeFile(
    join(sourceRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );
  const service = await createZvecGrep({
    root: sourceRoot,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await service.index();
  await service.close();
  const manifest = readWorkspaceManifest(join(sourceRoot, ".zvec-grep"));
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id);
  return { sourceRoot, legacyHome };
}

test("reservation blocks competing writers and discovery during migration", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-block-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const destinationRoot = join(parent, "destination");
  await mkdir(join(destinationRoot, "docs"), { recursive: true });
  await writeFile(
    join(destinationRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );

  const competitorEvidence = [];
  const result = await migrateWorkspaceIndex({
    sourceHome: legacyHome,
    destinationRoot,
    onProgress: (stage) => {
      if (stage === "write") {
        // A competing writer and a discovery-time guard both meet the
        // reservation while it is held.
        try {
          assertNoWriteLock(
            join(destinationRoot, ".zvec-grep", "locks", "home"),
            "competitor",
          );
          competitorEvidence.push("not-blocked");
        } catch (error) {
          competitorEvidence.push(error.code);
        }
      }
    },
  });
  assert.equal(result.verification.countsMatch, true);
  assert.deepEqual(competitorEvidence, ["ZVEC_GREP.ENGINE.LOCK.BUSY"]);

  // After commit the destination is a normal, usable index.
  const manifest = readWorkspaceManifest(join(destinationRoot, ".zvec-grep"));
  assert.ok(manifest?.embedding);
});

test("a replaced reservation aborts without merging or deleting foreign content", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-replace-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const destinationRoot = join(parent, "destination");
  await mkdir(join(destinationRoot, "docs"), { recursive: true });
  await writeFile(
    join(destinationRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );
  const destinationHome = join(destinationRoot, ".zvec-grep");

  await assert.rejects(
    migrateWorkspaceIndex({
      sourceHome: legacyHome,
      destinationRoot,
      onProgress: (stage) => {
        if (stage === "verify") {
          // Deliberate replacement of the reserved destination before
          // publication, synchronously so the publish step sees it. Churn
          // keeps the replacement's inode distinct; a same-inode replacement
          // is physically indistinguishable by construction.
          rmSync(destinationHome, { recursive: true, force: true });
          for (let i = 0; i < 64; i++) {
            mkdirSync(join(destinationRoot, `churn-${i}`));
          }
          mkdirSync(destinationHome, { recursive: true });
          writeFileSync(join(destinationHome, "sentinel.txt"), "foreign claim");
        }
      },
    }),
    /replaced before publication|RESERVATION|not exist/,
    "the operation must fail loudly when its reservation is replaced",
  );

  const entries = await readdir(destinationHome);
  assert.deepEqual(entries, ["sentinel.txt"]);
  assert.equal(
    await readFile(join(destinationHome, "sentinel.txt"), "utf8"),
    "foreign claim",
    "the replacement must be preserved exactly",
  );
});

test("an error after commit never touches the published result", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-commit-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const destinationRoot = join(parent, "destination");
  await mkdir(join(destinationRoot, "docs"), { recursive: true });
  await writeFile(
    join(destinationRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );

  await assert.rejects(
    migrateWorkspaceIndex({
      sourceHome: legacyHome,
      destinationRoot,
      onProgress: (stage) => {
        if (stage === "done") {
          throw new Error("injected post-commit failure");
        }
      },
    }),
    /injected post-commit failure/,
  );

  // The published destination survives with its complete content.
  const manifest = readWorkspaceManifest(join(destinationRoot, ".zvec-grep"));
  assert.ok(manifest?.embedding);
  const entries = await readdir(join(destinationRoot, ".zvec-grep"));
  assert.ok(entries.includes("files.zvec"));
  assert.ok(entries.includes("index.zvec"));
});

test("an incomplete destination stays blocked after lock cleanup, with no ancestor fallback", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-incomplete-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const destinationRoot = join(parent, "destination");
  await mkdir(join(destinationRoot, "docs"), { recursive: true });
  await writeFile(
    join(destinationRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );
  const destinationHome = join(destinationRoot, ".zvec-grep");

  // Simulate a crashed operation: the durable marker and a dead owner's
  // write lock persist; partial staged content is present.
  const lockDir = join(destinationHome, "locks", "home.write");
  await mkdir(lockDir, { recursive: true });
  await writeFile(
    join(lockDir, "lock.json"),
    `${JSON.stringify(
      {
        token: "crashed-token",
        pid: 99_999_999,
        hostname: (await import("node:os")).hostname(),
        startedAt: Date.now(),
        operation: "index.import",
      },
      null,
      2,
    )}\n`,
  );
  await mkdir(join(destinationHome, "staging-crashed"), { recursive: true });
  await writeFile(
    join(destinationHome, "INCOMPLETE"),
    `${JSON.stringify({ token: "crashed-token", operation: "index.import" })}\n`,
  );

  const service = await createZvecGrep({
    root: destinationRoot,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.context({ query: "guide", limit: 3 }),
    /incomplete|INCOMPLETE|Index unavailable/i,
    "readers are blocked while the abandoned reservation's lock persists",
  );
  await service.close();

  // The operator removes the lock directory during recovery; the marker
  // alone still blocks readers and writers (no ancestor fallback).
  await rm(join(destinationHome, "locks"), { recursive: true, force: true });
  const service2 = await createZvecGrep({
    root: destinationRoot,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service2.context({ query: "guide", limit: 3 }),
    /incomplete|INCOMPLETE/i,
    "the durable marker alone keeps the destination blocked",
  );
  await assert.rejects(
    service2.index(),
    /incomplete|INCOMPLETE|busy|BUSY/i,
    "writers are blocked by the durable marker",
  );
  await service2.close();

  // Documented recovery: with writers quiescent, remove the marker and the
  // partial contents; the workspace is usable again.
  await rm(destinationHome, { recursive: true, force: true });
  const result = await migrateWorkspaceIndex({
    sourceHome: legacyHome,
    destinationRoot,
  });
  assert.equal(result.verification.countsMatch, true);
});

test("a crashed destination stays blocked even with an ancestor index present", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-ancestor-");

  // The parent workspace has a working index (the ancestor fallback).
  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await parentService.index();
  await parentService.close();

  // The child workspace has an incomplete reserved destination.
  const child = join(parent, "child");
  const childHome = join(child, ".zvec-grep");
  await mkdir(childHome, { recursive: true });
  await writeFile(join(child, "docs.md"), "# Child\n\nChild content.\n");
  await writeFile(
    join(childHome, "INCOMPLETE"),
    `${JSON.stringify({ token: "crashed", operation: "index.import" })}\n`,
  );

  // Discovery from inside the child must not fall back to the parent's index.
  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    /incomplete|INCOMPLETE/i,
    "the incomplete child must block instead of falling back to the ancestor",
  );
  await service.close();

  // The parent's own index still works normally.
  const parentSearch = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  const result = await parentSearch.context({
    query: "ancestor content",
    limit: 3,
  });
  assert.ok(result.items.length > 0);
  await parentSearch.close();
});

test("ownership loss during finalization publishes nothing", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-finalize-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.finalize-loss",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "staged");
  const lockDir = join(destinationHome, "locks", "home.write");

  await assert.rejects(
    Promise.resolve().then(() =>
      reservation.publish(() => {
        // Ownership is lost while finalization runs.
        rmSync(lockDir, { recursive: true, force: true });
        mkdirSync(lockDir, { recursive: true });
        writeFileSync(
          join(lockDir, "lock.json"),
          `${JSON.stringify({ token: "competitor" })}\n`,
        );
      }),
    ),
    /ownership.*lost|RESERVATION/i,
  );
  const destinationEntries = await readdir(destinationHome);
  assert.ok(!destinationEntries.includes("manifest.json"));
  assert.ok(
    !destinationEntries.includes("payload.txt"),
    "nothing was published into the compromised destination",
  );
  assert.deepEqual(
    JSON.parse(await readFile(join(lockDir, "lock.json"), "utf8")).token,
    "competitor",
    "the replacement lock survives the fenced publication",
  );
});

test("an aborted staging replacement preserves the foreign file", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-staging-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.staging-replace",
    existingIndexMarkers: ["manifest.json"],
  });

  // Replace only the staging directory with foreign content.
  rmSync(reservation.stagingHome, { recursive: true, force: true });
  for (let i = 0; i < 64; i++) {
    mkdirSync(join(parent, `churn-${i}`));
  }
  mkdirSync(reservation.stagingHome, { recursive: true });
  writeFileSync(join(reservation.stagingHome, "foreign.txt"), "not ours");

  const cleanup = reservation.abort();
  assert.equal(
    await readFile(join(reservation.stagingHome, "foreign.txt"), "utf8"),
    "not ours",
    "abort must not delete a staging replacement",
  );
  // Skipped cleanup is unfinished cleanup: the foreign content is preserved,
  // the marker is preserved as blockage, and the state is reported.
  assert.match(cleanup, /replaced by foreign content/i);
  assert.match(cleanup, /blocked by the INCOMPLETE marker/i);
  assert.ok(existsSync(join(destinationHome, "INCOMPLETE")));
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
});

test("an abandoned reservation stays blocked until the documented operator recovery", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-abandon-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const destinationRoot = join(parent, "destination");
  await mkdir(join(destinationRoot, "docs"), { recursive: true });
  await writeFile(
    join(destinationRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );

  // Simulate an abandoned reservation: a write lock whose owner is dead.
  // Automatic reclamation is intentionally removed, so this stays blocked.
  const lockDir = join(destinationRoot, ".zvec-grep", "locks", "home.write");
  await mkdir(lockDir, { recursive: true });
  await writeFile(
    join(lockDir, "lock.json"),
    `${JSON.stringify(
      {
        token: "abandoned-token",
        pid: 99_999_999,
        hostname: (await import("node:os")).hostname(),
        startedAt: Date.now(),
        operation: "index.migrate",
      },
      null,
      2,
    )}\n`,
  );

  await assert.rejects(
    migrateWorkspaceIndex({
      sourceHome: legacyHome,
      destinationRoot,
    }),
    (error) => error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY",
    "an abandoned write lock blocks until operator recovery",
  );

  // Documented recovery: with writers quiescent, the operator removes the
  // lock directory; the operation then proceeds.
  await rm(join(destinationRoot, ".zvec-grep", "locks"), {
    recursive: true,
    force: true,
  });
  const result = await migrateWorkspaceIndex({
    sourceHome: legacyHome,
    destinationRoot,
  });
  assert.equal(result.verification.countsMatch, true);
});

test("export rejects a destination with unrelated contents without touching them", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-export-");
  const sourceRoot = join(parent, "original");
  await mkdir(join(sourceRoot, "docs"), { recursive: true });
  await writeFile(
    join(sourceRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );
  const service = await createZvecGrep({
    root: sourceRoot,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await service.index();
  await service.close();

  // A competitor already claimed the artifact path with its own content:
  // export must reject the destination and preserve the foreign files.
  const claimedArtifact = join(parent, "claimed-artifact");
  await mkdir(claimedArtifact, { recursive: true });
  await writeFile(join(claimedArtifact, "owner.txt"), "competitor");
  await assert.rejects(
    exportWorkspaceIndex({
      sourceHome: join(sourceRoot, ".zvec-grep"),
      artifactPath: claimedArtifact,
    }),
    /unrelated contents/,
  );
  assert.equal(
    await readFile(join(claimedArtifact, "owner.txt"), "utf8"),
    "competitor",
  );
  assert.ok(
    !(await readdir(claimedArtifact)).includes("entities.jsonl"),
    "no export content is written beside the foreign claim",
  );

  // A foreign colliding child inside the reserved destination is rejected
  // at publication and never overwritten (direct protocol exercise).
  const collisionHome = join(parent, "collision-home");
  const reservation = reserveDestination({
    destinationHome: collisionHome,
    operation: "test.collision",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "entities.jsonl"), "ours\n");
  await writeFile(join(collisionHome, "entities.jsonl"), "foreign entities\n");
  assert.throws(
    () => reservation.publish(() => undefined),
    /already exists|refusing to overwrite/i,
  );
  assert.equal(
    await readFile(join(collisionHome, "entities.jsonl"), "utf8"),
    "foreign entities\n",
    "the colliding foreign child is never overwritten",
  );
  reservation.abort();
});

test("a lost reservation lock fences publication and abort preserves foreign data", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-lostlock-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const destinationRoot = join(parent, "destination");
  await mkdir(join(destinationRoot, "docs"), { recursive: true });
  await writeFile(
    join(destinationRoot, "docs", "guide.md"),
    FIXTURES["docs/guide.md"],
  );
  const destinationHome = join(destinationRoot, ".zvec-grep");
  const lockDir = join(destinationHome, "locks", "home.write");

  const replaceLock = () => {
    rmSync(lockDir, { recursive: true, force: true });
    for (let i = 0; i < 64; i++) {
      mkdirSync(join(destinationRoot, `churn-${i}`));
    }
    mkdirSync(lockDir, { recursive: true });
    writeFileSync(
      join(lockDir, "lock.json"),
      `${JSON.stringify(
        {
          token: "competitor-token",
          pid: process.pid,
          hostname: "this-host",
          startedAt: Date.now(),
          operation: "competitor",
        },
        null,
        2,
      )}\n`,
    );
    writeFileSync(join(destinationHome, "foreign.txt"), "competitor data");
  };

  // Publication with a replaced lock: fenced, competitor's lock preserved,
  // nothing merged.
  await assert.rejects(
    migrateWorkspaceIndex({
      sourceHome: legacyHome,
      destinationRoot,
      onProgress: (stage) => {
        if (stage === "verify") {
          replaceLock();
        }
      },
    }),
    /ownership.*lost|RESERVATION/i,
  );
  assert.equal(
    await readFile(join(destinationHome, "foreign.txt"), "utf8"),
    "competitor data",
  );
  assert.deepEqual(
    JSON.parse(await readFile(join(lockDir, "lock.json"), "utf8")).token,
    "competitor-token",
    "the competitor's lock must survive the fenced publication",
  );
  assert.ok(
    !(await readdir(destinationHome)).includes("manifest.json"),
    "nothing is published after ownership loss",
  );
});

test("import honors the artifact's writer lock", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-artifact-lock-");
  const { legacyHome } = await makeLegacySource(t, parent);
  const artifact = join(parent, "artifact");
  await exportWorkspaceIndex({
    sourceHome: legacyHome,
    artifactPath: artifact,
  });

  const { acquireReadWriteLock } =
    await import("../../dist/engine/utils/lock.js");
  const writer = acquireReadWriteLock(
    join(artifact, "locks", "home"),
    "write",
    { operation: "index.export" },
  );
  try {
    await assert.rejects(
      importWorkspaceIndex({
        artifactPath: artifact,
        destinationRoot: join(parent, "destination"),
      }),
      (error) => error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY",
      "import must not consume an artifact while its writer lock is held",
    );
  } finally {
    writer.release();
  }

  // After the writer releases (commit), the same artifact imports cleanly.
  const result = await importWorkspaceIndex({
    artifactPath: artifact,
    destinationRoot: join(parent, "destination"),
  });
  assert.equal(result.verification.countsMatch, true);
});

test("a failed rollback keeps the destination blocked instead of serving the ancestor", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-rollback-");

  // Ancestor workspace with a working index.
  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await parentService.index();
  await parentService.close();

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });
  await writeFile(join(child, "docs.md"), "# Child\n\nChild content.\n");

  const danglingTarget = join(parent, "no-such-target");
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.failed-rollback",
    existingIndexMarkers: ["manifest.json"],
    testHooks: {
      afterChildMove(child) {
        if (child !== "payload.txt") {
          return;
        }
        // Between the first move and the next: a foreign dangling link
        // appears at the moved child's staging target, and a foreign file
        // occupies the next move's destination. Rollback must refuse to
        // overwrite the link instead of carrying it into staging.
        symlinkSync(danglingTarget, join(reservation.stagingHome, child));
        writeFileSync(join(destinationHome, "manifest.json"), "foreign\n");
      },
    },
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  assert.throws(
    () =>
      reservation.publish(() => {
        writeFileSync(join(reservation.stagingHome, "manifest.json"), "{}");
      }),
    /rollback is incomplete/i,
  );

  // The foreign link is preserved at the staging target; the published child
  // remains at the destination (not moved back, not deleted); the foreign
  // destination entry is preserved.
  assert.equal(
    readlinkSync(join(reservation.stagingHome, "payload.txt")),
    danglingTarget,
  );
  assert.equal(
    await readFile(join(destinationHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.equal(
    await readFile(join(destinationHome, "manifest.json"), "utf8"),
    "foreign\n",
  );
  // Blockage is preserved by the durable marker.
  assert.ok(existsSync(join(destinationHome, "INCOMPLETE")));
  // Readers from the child are blocked rather than served by the ancestor.
  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    /incomplete|INCOMPLETE/i,
    "a failed rollback must never expose the ancestor fallback",
  );
  await service.close();

  // Abort after the failed publish does not weaken the blockage.
  reservation.abort();
  assert.ok(
    lstatSync(join(reservation.stagingHome, "payload.txt")).isSymbolicLink(),
  );
  assert.ok(existsSync(join(destinationHome, "INCOMPLETE")));
});

test("publication fails when the marker is replaced by foreign state", async (t) => {
  const parent = await createTemporaryDirectory(
    t,
    "zg-reserve-foreign-marker-",
  );
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.foreign-marker",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");
  const foreignMarker = `${JSON.stringify({ token: "foreign-token" })}\n`;

  assert.throws(
    () =>
      reservation.publish(() => {
        writeFileSync(join(reservation.stagingHome, "manifest.json"), "{}");
        writeFileSync(join(destinationHome, "INCOMPLETE"), foreignMarker);
      }),
    /replaced by foreign state/i,
  );

  // The foreign marker is preserved exactly; publication rolled back.
  assert.equal(
    await readFile(join(destinationHome, "INCOMPLETE"), "utf8"),
    foreignMarker,
  );
  assert.equal(
    await readFile(join(reservation.stagingHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.ok(!existsSync(join(destinationHome, "payload.txt")));
  // The destination remains blocked for readers, and the lock was released.
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));
});

test("publication fails when the marker vanishes and restores the blockage", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-lost-marker-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.lost-marker",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  assert.throws(
    () =>
      reservation.publish(() => {
        writeFileSync(join(reservation.stagingHome, "manifest.json"), "{}");
        rmSync(join(destinationHome, "INCOMPLETE"));
      }),
    /unexpectedly absent/i,
  );

  // Blockage is restored with this reservation's token; payload rolled back.
  const marker = JSON.parse(
    await readFile(join(destinationHome, "INCOMPLETE"), "utf8"),
  );
  assert.equal(marker.operation, "rollback-block");
  assert.equal(
    await readFile(join(reservation.stagingHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
});

test("an incomplete rollback with a missing marker restores the marker and reports recovery", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-remarker-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const danglingTarget = join(parent, "no-such-target");
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.remarker",
    existingIndexMarkers: ["manifest.json"],
    testHooks: {
      afterChildMove(child) {
        // Block the child's return and remove the durable marker.
        symlinkSync(danglingTarget, join(reservation.stagingHome, child));
        rmSync(join(destinationHome, "INCOMPLETE"));
      },
    },
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  assert.throws(
    () => reservation.publish(() => undefined),
    /rollback is incomplete/i,
  );

  // The marker is restored (ownership was verifiable) and the destination
  // stays blocked with the partial payload in place; the foreign link at the
  // staging target is preserved.
  const marker = JSON.parse(
    await readFile(join(destinationHome, "INCOMPLETE"), "utf8"),
  );
  assert.equal(marker.operation, "rollback-block");
  assert.equal(
    readlinkSync(join(reservation.stagingHome, "payload.txt")),
    danglingTarget,
  );
  assert.equal(
    await readFile(join(destinationHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
});

test("an incomplete rollback preserves a foreign marker and releases the lock", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-foreignkeep-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const danglingTarget = join(parent, "no-such-target");
  const foreignMarker = `${JSON.stringify({ token: "foreign-token" })}\n`;
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.foreignkeep",
    existingIndexMarkers: ["manifest.json"],
    testHooks: {
      afterChildMove(child) {
        symlinkSync(danglingTarget, join(reservation.stagingHome, child));
        writeFileSync(join(destinationHome, "INCOMPLETE"), foreignMarker);
      },
    },
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  assert.throws(
    () => reservation.publish(() => undefined),
    /rollback is incomplete/i,
  );

  // The foreign marker is never overwritten; the lock is released because
  // the foreign marker itself blocks the destination.
  assert.equal(
    await readFile(join(destinationHome, "INCOMPLETE"), "utf8"),
    foreignMarker,
  );
  assert.equal(
    readlinkSync(join(reservation.stagingHome, "payload.txt")),
    danglingTarget,
  );
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
});

// Fixture-owned, explicitly ordered teardown: restore known protected
// paths, finalize the owned reservation (abort's result is inspected — a
// diagnostic string means incomplete cleanup and is surfaced), then remove
// the temporary tree. Registered as soon as resources are owned and the
// fixture's automatic removal is disabled; the returned function is
// idempotent so controls can await it directly after an injected failure.
const probeInjections = new Map();

function expectOwnedService(owned, service) {
  // Control-side record, written at acquisition and independent of the
  // teardown registration: removing a registration must not remove the
  // control's expected resource.
  (owned.expectedServices ??= []).push(service);
}

function trackOwnedService(owned, service) {
  owned.services.push(service);
  observeOwnedClose(owned, service);
}

function markOwnedServiceClosed(owned, service) {
  owned.closedServices.add(service);
}

function expectOwnedReservation(owned, reservation) {
  // Control-side record, independent of the teardown registration.
  (owned.expectedReservations ??= []).push(reservation);
}

function trackOwnedReservation(owned, reservation) {
  owned.reservations.push(reservation);
  observeOwnedAbort(owned, reservation);
}

// The observers wrap the real resource methods. A control therefore sees
// the actual close or abort call, the resource identity, the phase and the
// position of the call relative to the tree removal. Records that only
// teardown writes never satisfy a control.
function observeOwnedClose(owned, service) {
  owned.closeCalls ??= new Map();
  owned.operationLog ??= [];
  if (owned.closeCalls.has(service)) {
    return;
  }
  owned.closeCalls.set(service, 0);
  const originalClose = service.close.bind(service);
  service.close = async (...args) => {
    let ok = true;
    try {
      return await originalClose(...args);
    } catch (error) {
      ok = false;
      throw error;
    } finally {
      owned.closeCalls.set(service, owned.closeCalls.get(service) + 1);
      owned.operationLog.push({
        op: "close",
        resource: service,
        phase: owned.inTeardown ? "teardown" : "inline",
        ok,
      });
    }
  };
}

function observeOwnedAbort(owned, reservation) {
  owned.abortCalls ??= new Map();
  owned.operationLog ??= [];
  if (owned.abortCalls.has(reservation)) {
    return;
  }
  owned.abortCalls.set(reservation, 0);
  const originalAbort = reservation.abort.bind(reservation);
  reservation.abort = (...args) => {
    let ok = true;
    let result;
    try {
      result = originalAbort(...args);
    } catch (error) {
      ok = false;
      throw error;
    } finally {
      owned.abortCalls.set(reservation, owned.abortCalls.get(reservation) + 1);
      owned.operationLog.push({
        op: "abort",
        resource: reservation,
        phase: owned.inTeardown ? "teardown" : "inline",
        ok,
      });
    }
    return result;
  };
}

function assertOwnedResourcesFinalized(owned) {
  // Every expected service must have one successful close call, and every
  // expected reservation one successful abort call. Each call is observed
  // on the real method and must be recorded before the tree removal, which
  // would otherwise destroy the lock and staging evidence.
  const log = owned.operationLog ?? [];
  const treeIndex = log.findIndex((entry) => entry.op === "tree");
  for (const service of owned.expectedServices ?? []) {
    const closeIndex = log.findIndex(
      (entry) => entry.op === "close" && entry.resource === service && entry.ok,
    );
    assert.ok(
      closeIndex !== -1,
      "each expected service was closed (observed on the real method)",
    );
    if (treeIndex !== -1) {
      assert.ok(
        closeIndex < treeIndex,
        "each expected service was closed before the tree removal",
      );
    }
  }
  for (const reservation of owned.expectedReservations ?? []) {
    const abortIndex = log.findIndex(
      (entry) =>
        entry.op === "abort" && entry.resource === reservation && entry.ok,
    );
    assert.ok(
      abortIndex !== -1,
      "each expected reservation was aborted (observed on the real method)",
    );
    if (treeIndex !== -1) {
      assert.ok(
        abortIndex < treeIndex,
        "each expected reservation was aborted before the tree removal",
      );
    }
  }
}

function ownReservationTeardown(t, owned) {
  owned.services = owned.services ?? [];
  owned.closedServices = owned.closedServices ?? new Set();
  owned.reservations = owned.reservations ?? [];
  owned.abortResults = owned.abortResults ?? [];
  owned.operationLog = owned.operationLog ?? [];
  // Registered immediately after temporary allocation, before any fallible
  // setup; the owned state grows as resources are acquired.
  const teardown = async () => {
    if (owned.done) {
      return;
    }
    owned.done = true;
    owned.inTeardown = true;
    const errors = [];
    for (const entry of owned.protected ?? []) {
      try {
        if (existsSync(entry.path)) {
          chmodSync(entry.path, entry.mode);
        }
      } catch (error) {
        errors.push(error);
      }
    }
    for (const service of owned.services ?? []) {
      // Close only services that are still open. A service that the test
      // body closed successfully is marked in owned.closedServices.
      if (owned.closedServices.has(service)) {
        continue;
      }
      try {
        await service.close();
        owned.closedServices.add(service);
      } catch (error) {
        errors.push(error);
      }
    }
    for (const reservation of owned.reservations) {
      try {
        const result = reservation.abort();
        owned.abortResults.push(result ?? null);
        if (result !== undefined && result !== null) {
          t.diagnostic(`teardown abort reported incomplete cleanup: ${result}`);
        }
      } catch (error) {
        errors.push(error);
      }
    }
    try {
      await rm(owned.tempDir, { recursive: true, force: true });
      owned.operationLog.push({ op: "tree", ok: true });
    } catch (error) {
      owned.operationLog.push({ op: "tree", ok: false });
      errors.push(error);
    }
    if (errors.length > 0) {
      throw new Error(
        `fixture teardown failed: ${errors.map((e) => e.message).join("; ")}`,
      );
    }
  };
  t.after(teardown);
  return teardown;
}

async function runRetainLockFixture(t, options = {}) {
  const owned = options.owned ?? {};
  const parent = await createTemporaryDirectory(t, "zg-reserve-retainlock-", {
    cleanup: false,
  });
  owned.tempDir = parent;
  owned.services = [];
  owned.teardown = ownReservationTeardown(t, owned);
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  owned.protected = [{ path: destinationHome, mode: 0o755 }];
  // Whether mode bits can make the home unwritable is a platform
  // capability, measured independently inside the hook. An unexpected
  // probe error is captured and rethrown by the test as a probe failure —
  // publication error handling must not reinterpret it.
  let homeUnwritable = false;
  let retainLockProbeFailure;
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.retainlock",
    existingIndexMarkers: ["manifest.json"],
    testHooks: {
      afterChildMove() {
        // The marker vanishes and the home becomes unwritable: rollback
        // cannot move the child back and the marker cannot be restored.
        rmSync(join(destinationHome, "INCOMPLETE"));
        chmodSync(destinationHome, 0o555);
        try {
          // Injections replace the filesystem attempt and flow through the
          // same classifier and required-denial guard as the real path.
          const attempt = probeInjections.get("retainlock");
          const outcome = guardedSyncPermissionProbe(
            t,
            "reservation.retainlock:write-home",
            attempt ??
              (() => writeFileSync(join(destinationHome, ".write-probe"), "")),
            attempt ? "injected" : "real",
          );
          if (outcome === "allowed") {
            rmSync(join(destinationHome, ".write-probe"), { force: true });
          }
          homeUnwritable = outcome === "denied";
        } catch (error) {
          retainLockProbeFailure = error;
          throw error;
        }
      },
    },
  });
  expectOwnedReservation(owned, reservation);
  trackOwnedReservation(owned, reservation);
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  try {
    reservation.publish(() => undefined);
    assert.fail("publication must fail");
  } catch (error) {
    if (retainLockProbeFailure) {
      throw retainLockProbeFailure;
    }
    assert.match(String(error), /unexpectedly absent/i);
  }
  // The unwritable home was only needed to fail the rollback and restore;
  // restore permissions before assertions and cleanup.
  chmodSync(destinationHome, 0o755);

  if (!homeUnwritable) {
    // Advisory mode bits: the intended fault never occurred; the engine
    // detects the vanished marker and rolls the publication back
    // completely, restoring the blockage (the "publication fails when
    // the marker vanishes" behavior).
    const marker = JSON.parse(
      await readFile(join(destinationHome, "INCOMPLETE"), "utf8"),
    );
    assert.equal(marker.operation, "rollback-block");
    assert.equal(
      await readFile(join(reservation.stagingHome, "payload.txt"), "utf8"),
      "ours",
    );
    assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
    return;
  }

  // The write lock is retained as the last block; nothing else was written.
  const lockInfo = JSON.parse(
    await readFile(
      join(destinationHome, "locks", "home.write", "lock.json"),
      "utf8",
    ),
  );
  assert.equal(typeof lockInfo.token, "string");
  assert.ok(!existsSync(join(destinationHome, "INCOMPLETE")));
  assert.equal(
    await readFile(join(destinationHome, "payload.txt"), "utf8"),
    "ours",
  );

  // Abort must not release the retained lock or weaken the blockage.
  reservation.abort();
  assert.ok(existsSync(join(destinationHome, "locks", "home.write")));
  assert.throws(
    () =>
      assertNoWriteLock(join(destinationHome, "locks", "home"), "competitor"),
    (error) => error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY",
    "the retained write lock keeps writers out",
  );

  // Discovery is blocked by the retained lock rather than falling back.
  const service = await createZvecGrep({
    root: join(parent, "destination"),
    embeddingModel: new FakeEmbeddingModel(),
  });
  expectOwnedService(owned, service);
  trackOwnedService(owned, service);
  await assert.rejects(
    service.context({ query: "anything", limit: 1 }),
    (error) => error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY",
  );
  await service.close();
  markOwnedServiceClosed(owned, service);

  // Documented operator recovery: quiesce writers, remove the lock directory
  // and the partial contents; publication then succeeds.
  await rm(join(destinationHome, "locks"), { recursive: true, force: true });
  await rm(join(destinationHome, "payload.txt"));
  await rm(reservation.stagingHome, { recursive: true, force: true });
  const recovered = reserveDestination({
    destinationHome,
    operation: "test.recovery",
    existingIndexMarkers: ["manifest.json"],
  });
  expectOwnedReservation(owned, recovered);
  trackOwnedReservation(owned, recovered);
  if (options.failAfterRecoveredReservation) {
    throw new Error("injected failure after the recovered reservation");
  }
  writeFileSync(join(recovered.stagingHome, "manifest.json"), "{}");
  recovered.publish(() => undefined);
  assert.ok(existsSync(join(destinationHome, "manifest.json")));
  return owned;
}

test("an incomplete rollback with an unrestorable marker retains the write lock", (t) =>
  runRetainLockFixture(t));

test("publication rejects a dangling destination child without overwriting it", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-dangling-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.dangling-target",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");
  // A foreign dangling link already occupies the publication target: it is
  // an occupied pathname, never an empty destination.
  const danglingTarget = join(parent, "no-such-target");
  symlinkSync(danglingTarget, join(destinationHome, "payload.txt"));

  assert.throws(
    () =>
      reservation.publish(() => {
        writeFileSync(join(reservation.stagingHome, "manifest.json"), "{}");
      }),
    /already exists|refusing to overwrite/i,
  );

  // The foreign link's target and entry identity are unchanged.
  assert.equal(
    readlinkSync(join(destinationHome, "payload.txt")),
    danglingTarget,
  );
  assert.ok(lstatSync(join(destinationHome, "payload.txt")).isSymbolicLink());
  // Nothing moved; the staged payload remains in staging and the destination
  // stays blocked.
  assert.equal(
    await readFile(join(reservation.stagingHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
});

test("publication validates every staged entry type before the first move", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-types-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.entry-types",
    existingIndexMarkers: ["manifest.json"],
  });
  // Directories are legitimate staged children (native collections); a
  // symlink is not. Validation happens before any move, so the legitimate
  // child must not move either.
  await mkdir(join(reservation.stagingHome, "collection.zvec"), {
    recursive: true,
  });
  const linkTarget = join(parent, "elsewhere.txt");
  await writeFile(linkTarget, "elsewhere");
  symlinkSync(linkTarget, join(reservation.stagingHome, "evil.txt"));

  assert.throws(
    () =>
      reservation.publish(() => {
        writeFileSync(join(reservation.stagingHome, "manifest.json"), "{}");
      }),
    /unsupported entry type/i,
  );

  assert.equal(
    readlinkSync(join(reservation.stagingHome, "evil.txt")),
    linkTarget,
  );
  assert.ok(
    !existsSync(join(destinationHome, "collection.zvec")),
    "no child moved before the type rejection",
  );
  assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
});

test("replacement staging installed before publish is rejected before any move", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-swap-pre-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.staging-swap-pre",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  // Move the original staging aside and install a replacement with foreign
  // content before publication starts.
  const asideHome = `${reservation.stagingHome}-aside`;
  renameSync(reservation.stagingHome, asideHome);
  await mkdir(reservation.stagingHome, { recursive: true });
  await writeFile(join(reservation.stagingHome, "foreign.txt"), "not ours");

  assert.throws(
    () => reservation.publish(() => undefined),
    /staging.*(lost|replaced)|ownership/i,
  );

  // The replacement's content is preserved in place; the original staging
  // is preserved aside; nothing was moved into the destination.
  assert.equal(
    await readFile(join(reservation.stagingHome, "foreign.txt"), "utf8"),
    "not ours",
  );
  assert.equal(await readFile(join(asideHome, "payload.txt"), "utf8"), "ours");
  assert.ok(!existsSync(join(destinationHome, "foreign.txt")));
  assert.ok(!existsSync(join(destinationHome, "payload.txt")));
});

test("replacement staging installed inside finalization is rejected before enumeration", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-reserve-swap-fin-");
  const destinationHome = join(parent, "destination", ".zvec-grep");
  await mkdir(join(parent, "destination"), { recursive: true });
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.staging-swap-finalize",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");
  const asideHome = `${reservation.stagingHome}-aside`;

  assert.throws(
    () =>
      reservation.publish(() => {
        renameSync(reservation.stagingHome, asideHome);
        mkdirSync(reservation.stagingHome, { recursive: true });
        writeFileSync(join(reservation.stagingHome, "foreign.txt"), "not ours");
      }),
    /staging.*(lost|replaced)|ownership/i,
  );

  assert.equal(
    await readFile(join(reservation.stagingHome, "foreign.txt"), "utf8"),
    "not ours",
  );
  assert.equal(await readFile(join(asideHome, "payload.txt"), "utf8"), "ours");
  assert.ok(!existsSync(join(destinationHome, "foreign.txt")));
});

test("a complete rollback with an unreadable marker stays blocked with a live ancestor", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-unread-marker-");

  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await parentService.index();
  await parentService.close();

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });
  await writeFile(join(child, "docs.md"), "# Child\n\nChild content.\n");

  const danglingMarkerTarget = join(parent, "no-such-marker-target");
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.unreadable-marker",
    existingIndexMarkers: ["manifest.json"],
    testHooks: {
      afterChildMove() {
        // Replace the marker with a dangling symlink: present as an entry,
        // unreadable as a marker, and never overwritten.
        rmSync(join(destinationHome, "INCOMPLETE"));
        symlinkSync(danglingMarkerTarget, join(destinationHome, "INCOMPLETE"));
      },
    },
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  assert.throws(
    () => reservation.publish(() => undefined),
    /unreadable at publication/i,
  );

  // The payload rolled back into staging; the unreadable marker entry is
  // preserved exactly; the lock was released because the entry blocks.
  assert.equal(
    await readFile(join(reservation.stagingHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.equal(
    readlinkSync(join(destinationHome, "INCOMPLETE")),
    danglingMarkerTarget,
  );
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));

  // Readers from the child are blocked rather than served by the ancestor.
  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    /incomplete|INCOMPLETE/i,
    "an unreadable marker entry must keep blocking discovery",
  );
  await service.close();
});

async function runAbortCleanupFixture(t, options = {}) {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const owned = options.owned ?? {};
  const parent = await createTemporaryDirectory(t, "zg-reserve-abortcleanup-", {
    cleanup: false,
  });
  owned.tempDir = parent;
  owned.services = [];
  owned.teardown = ownReservationTeardown(t, owned);
  if (options.failDuringSetup) {
    throw new Error("injected setup failure before any further acquisition");
  }

  // Live ancestor index.
  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  expectOwnedService(owned, parentService);
  trackOwnedService(owned, parentService);
  await parentService.index();
  await parentService.close();
  markOwnedServiceClosed(owned, parentService);

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });
  await writeFile(join(child, "docs.md"), "# Child\n\nChild content.\n");

  const reservation = reserveDestination({
    destinationHome,
    operation: "test.abort-cleanup",
    existingIndexMarkers: ["manifest.json"],
  });
  expectOwnedReservation(owned, reservation);
  trackOwnedReservation(owned, reservation);
  if (options.failAfterReservation) {
    throw new Error("injected setup failure after reservation acquisition");
  }
  // Real permission failure, no syscall mocking: a read-only staged
  // subdirectory cannot be emptied, so recursive staging removal fails.
  const protectedDir = join(reservation.stagingHome, "protected");
  await mkdir(protectedDir, { recursive: true });
  await writeFile(
    join(protectedDir, "payload.txt"),
    "owned recoverable payload",
  );
  owned.protected = [{ path: protectedDir, mode: 0o700 }];
  chmodSync(protectedDir, 0o500);
  const fs = await import("node:fs/promises");
  const injectedAttempt = probeInjections.get("abort-cleanup");
  const unlinkOutcome = await probeDenial(
    t,
    "reservation.abort-cleanup:unlink-payload",
    injectedAttempt ?? (() => fs.unlink(join(protectedDir, "payload.txt"))),
    injectedAttempt ? "injected" : "real",
  );
  const unlinkControl = unlinkOutcome === "denied" ? "EACCES" : undefined;
  t.diagnostic(
    `permission-branch operation=reservation.abort-cleanup branch=${
      unlinkOutcome === "denied" ? "strict" : "advisory"
    }`,
  );
  assertDenialInducible(t, "reservation.abort-cleanup", unlinkOutcome);

  const cleanup = reservation.abort();
  // Restore normal access before any discovery check: permission failure
  // itself must not be mistaken for persistent protection.
  if (existsSync(protectedDir)) {
    chmodSync(protectedDir, 0o700);
  }

  if (unlinkControl !== "EACCES") {
    // Advisory mode bits: the staged payload is removable, so the cleanup
    // completes — staging is emptied and the blockage lifts.
    assert.equal(cleanup, undefined);
    assert.ok(!existsSync(protectedDir));
    assert.ok(!existsSync(join(destinationHome, "INCOMPLETE")));
    assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));
    return;
  }

  // The cleanup failure is reported (the errno is the platform's cleanup
  // failure: EACCES from the denied unlink on Linux, ENOTEMPTY from
  // macOS's recursive removal), the payload remains, the marker is
  // preserved as blockage, and the lock is released against it.
  assert.match(cleanup, /could not be removed \((EACCES|ENOTEMPTY)\)/i);
  assert.match(cleanup, /blocked by the INCOMPLETE marker/i);
  assert.ok(existsSync(join(protectedDir, "payload.txt")));
  assert.ok(existsSync(join(destinationHome, "INCOMPLETE")));
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));

  // With access restored and a live ancestor, writers and discovery are
  // denied; the ancestor is not served.
  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  expectOwnedService(owned, service);
  trackOwnedService(owned, service);
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    /incomplete|INCOMPLETE/i,
  );
  if (options.failAfterDiscoveryService) {
    throw new Error("injected failure with the discovery service open");
  }
  await service.close();
  markOwnedServiceClosed(owned, service);

  // Documented operator recovery: remove the marker and partial contents,
  // then the workspace publishes normally.
  await rm(join(destinationHome, "INCOMPLETE"));
  await rm(reservation.stagingHome, { recursive: true, force: true });
  const recovered = reserveDestination({
    destinationHome,
    operation: "test.recovery",
    existingIndexMarkers: ["manifest.json"],
  });
  expectOwnedReservation(owned, recovered);
  trackOwnedReservation(owned, recovered);
  if (options.failAfterRecoveredReservation) {
    throw new Error("injected failure after the recovered reservation");
  }
  writeFileSync(join(recovered.stagingHome, "manifest.json"), "{}");
  recovered.publish(() => undefined);
  assert.ok(existsSync(join(destinationHome, "manifest.json")));
  return owned;
}

test("abort with an unremovable staging payload preserves blockage and reports it", (t) =>
  runAbortCleanupFixture(t));

async function runPermissionRecoveryFixture(t, options = {}) {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const owned = options.owned ?? {};
  const parent = await createTemporaryDirectory(t, "zg-reserve-permrec-", {
    cleanup: false,
  });
  owned.tempDir = parent;
  owned.services = [];
  owned.teardown = ownReservationTeardown(t, owned);

  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  expectOwnedService(owned, parentService);
  trackOwnedService(owned, parentService);
  await parentService.index();
  await parentService.close();
  markOwnedServiceClosed(owned, parentService);

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });

  // Permission-failure and recovery coverage: an unreadable home also hides
  // the lock metadata, so even the release path refuses deletion. This
  // fixture cannot distinguish the marker decision itself — that is the
  // pinned marker-only inspection probe's role (validation evidence).
  // Whether mode bits can make the home unreadable is a platform
  // capability, measured independently inside the hook.
  let homeUnreadable = false;
  let permRecProbeFailure;
  const reservation = reserveDestination({
    destinationHome,
    operation: "test.permission-recovery",
    existingIndexMarkers: ["manifest.json"],
    testHooks: {
      afterChildMove() {
        rmSync(join(destinationHome, "INCOMPLETE"));
        chmodSync(destinationHome, 0o000);
        try {
          const attempt = probeInjections.get("permission-recovery");
          const outcome = guardedSyncPermissionProbe(
            t,
            "reservation.permission-recovery:readdir-home",
            attempt ?? (() => readdirSync(destinationHome)),
            attempt ? "injected" : "real",
          );
          homeUnreadable = outcome === "denied";
        } catch (error) {
          permRecProbeFailure = error;
          throw error;
        }
      },
    },
  });
  owned.protected = [{ path: destinationHome, mode: 0o755 }];
  expectOwnedReservation(owned, reservation);
  trackOwnedReservation(owned, reservation);
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  let publishError;
  try {
    reservation.publish(() => undefined);
  } catch (error) {
    publishError = error;
  }
  if (permRecProbeFailure) {
    throw permRecProbeFailure;
  }
  assert.ok(publishError, "publication must fail");
  // Denied mode bits make the marker uninspectable ("ownership was lost";
  // retained lock); advisory mode bits leave the marker visibly absent
  // ("unexpectedly absent"; complete rollback). The outcome decides.
  assert.match(
    String(publishError),
    homeUnreadable ? /retained as the last block/i : /unexpectedly absent/i,
  );
  // Restore normal access before any discovery check.
  chmodSync(destinationHome, 0o755);

  if (!homeUnreadable) {
    // Advisory mode bits: the intended fault never occurred; the engine
    // rolls the publication back completely and restores the blockage
    // (the "publication fails when the marker vanishes" behavior). The
    // retained-lock and ancestor-recovery coverage below stays on the
    // denied branch; the rollback-interference path is deterministic via
    // the vanished-marker test.
    const marker = JSON.parse(
      await readFile(join(destinationHome, "INCOMPLETE"), "utf8"),
    );
    assert.equal(marker.operation, "rollback-block");
    assert.equal(
      await readFile(join(reservation.stagingHome, "payload.txt"), "utf8"),
      "ours",
    );
    assert.throws(() => readWorkspaceManifest(destinationHome), /incomplete/i);
    return;
  }

  // The lock is retained, the marker is absent, the payload remains at the
  // destination, and writers and discovery stay denied with a live ancestor.
  assert.ok(existsSync(join(destinationHome, "locks", "home.write")));
  assert.ok(!existsSync(join(destinationHome, "INCOMPLETE")));
  assert.equal(
    await readFile(join(destinationHome, "payload.txt"), "utf8"),
    "ours",
  );
  assert.throws(
    () =>
      assertNoWriteLock(join(destinationHome, "locks", "home"), "competitor"),
    (error) => error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY",
  );
  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  expectOwnedService(owned, service);
  trackOwnedService(owned, service);
  if (options.failAfterDiscoveryService) {
    throw new Error("injected failure with the recovery service open");
  }
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    (error) => error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY",
  );
  await service.close();
  markOwnedServiceClosed(owned, service);

  // Documented operator recovery: quiesce writers, remove the lock directory
  // and the partial contents; the workspace then serves the ancestor again.
  await rm(join(destinationHome, "locks"), { recursive: true, force: true });
  await rm(join(destinationHome, "payload.txt"));
  const recovered = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  expectOwnedService(owned, recovered);
  trackOwnedService(owned, recovered);
  const result = await recovered.context({
    query: "ancestor content",
    limit: 3,
  });
  assert.ok(
    result.items.length > 0,
    "after recovery the ancestor serves again",
  );
  await recovered.close();
  markOwnedServiceClosed(owned, recovered);
  return owned;
}

test("a permission failure retains the write lock and operator recovery restores the workspace", (t) =>
  runPermissionRecoveryFixture(t));

test("abort preserves a staging symlink alias and the moved-aside payload", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-alias-");

  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await parentService.index();
  await parentService.close();

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });

  const reservation = reserveDestination({
    destinationHome,
    operation: "test.staging-alias",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(
    join(reservation.stagingHome, "payload.txt"),
    "owned payload",
  );
  // Move the owned staging aside inside the destination, then alias the
  // original path to it. The alias is a replacement entry: never deleted,
  // never treated as the owned staging.
  const asideHome = join(destinationHome, "staging-aside");
  renameSync(reservation.stagingHome, asideHome);
  symlinkSync(asideHome, reservation.stagingHome);

  const cleanup = reservation.abort();

  assert.match(cleanup, /symlink/i);
  assert.match(cleanup, /blocked by the INCOMPLETE marker/i);
  assert.equal(readlinkSync(reservation.stagingHome), asideHome);
  assert.equal(
    await readFile(join(asideHome, "payload.txt"), "utf8"),
    "owned payload",
  );
  assert.ok(existsSync(join(destinationHome, "INCOMPLETE")));
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));

  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    /incomplete|INCOMPLETE/i,
    "an aliased staging replacement must keep the destination blocked",
  );
  await service.close();

  // Documented operator recovery: remove the marker, the alias and the
  // moved-aside payload; publication then succeeds.
  await rm(join(destinationHome, "INCOMPLETE"));
  rmSync(reservation.stagingHome);
  await rm(asideHome, { recursive: true, force: true });
  const recovered = reserveDestination({
    destinationHome,
    operation: "test.recovery",
    existingIndexMarkers: ["manifest.json"],
  });
  writeFileSync(join(recovered.stagingHome, "manifest.json"), "{}");
  recovered.publish(() => undefined);
  assert.ok(existsSync(join(destinationHome, "manifest.json")));
});

test("abort treats a vanished staging directory as unfinished cleanup", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-vanished-");

  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await parentService.index();
  await parentService.close();

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });

  const reservation = reserveDestination({
    destinationHome,
    operation: "test.staging-vanished",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(
    join(reservation.stagingHome, "payload.txt"),
    "owned payload",
  );
  // The staging pathname disappears without a replacement; its payload
  // remains elsewhere in the destination.
  const asideHome = join(destinationHome, "staging-aside");
  renameSync(reservation.stagingHome, asideHome);

  const cleanup = reservation.abort();

  assert.match(cleanup, /unexpectedly absent/i);
  assert.match(cleanup, /blocked by the INCOMPLETE marker/i);
  assert.equal(
    await readFile(join(asideHome, "payload.txt"), "utf8"),
    "owned payload",
  );
  assert.ok(existsSync(join(destinationHome, "INCOMPLETE")));
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));

  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.context({ query: "ancestor content", limit: 3 }),
    /incomplete|INCOMPLETE/i,
    "unexplained staging disappearance must keep the destination blocked",
  );
  await service.close();

  await rm(join(destinationHome, "INCOMPLETE"));
  await rm(asideHome, { recursive: true, force: true });
  const recovered = reserveDestination({
    destinationHome,
    operation: "test.recovery",
    existingIndexMarkers: ["manifest.json"],
  });
  writeFileSync(join(recovered.stagingHome, "manifest.json"), "{}");
  recovered.publish(() => undefined);
  assert.ok(existsSync(join(destinationHome, "manifest.json")));
});

test("a normal abort cleans up completely and releases the workspace", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-reserve-clean-abort-");

  await mkdir(join(parent, "docs"), { recursive: true });
  await writeFile(
    join(parent, "docs", "ancestor.md"),
    "# Ancestor\n\nAncestor content.\n",
  );
  const parentService = await createZvecGrep({
    root: parent,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await parentService.index();
  await parentService.close();

  const child = join(parent, "child");
  const destinationHome = join(child, ".zvec-grep");
  await mkdir(child, { recursive: true });

  const reservation = reserveDestination({
    destinationHome,
    operation: "test.clean-abort",
    existingIndexMarkers: ["manifest.json"],
  });
  await writeFile(join(reservation.stagingHome, "payload.txt"), "ours");

  const cleanup = reservation.abort();

  // Verified complete cleanup: nothing blocks, nothing remains, no warning.
  assert.equal(cleanup, undefined);
  assert.ok(!existsSync(reservation.stagingHome));
  assert.ok(!existsSync(join(destinationHome, "INCOMPLETE")));
  assert.ok(!existsSync(join(destinationHome, "locks", "home.write")));

  // A clean workspace legitimately falls back to the ancestor index.
  const service = await createZvecGrep({
    root: child,
    embeddingModel: new FakeEmbeddingModel(),
  });
  const result = await service.context({ query: "ancestor content", limit: 3 });
  assert.ok(result.items.length > 0);
  await service.close();

  // And a fresh reservation publishes normally.
  const recovered = reserveDestination({
    destinationHome,
    operation: "test.recovery",
    existingIndexMarkers: ["manifest.json"],
  });
  writeFileSync(join(recovered.stagingHome, "manifest.json"), "{}");
  recovered.publish(() => undefined);
  assert.ok(existsSync(join(destinationHome, "manifest.json")));
});

test("retain-lock fixture fails as a probe failure and cleans up automatically", async (t) => {
  const owned = {};
  const injected = new Error("injected probe failure");
  injected.code = "EIO";
  probeInjections.set("retainlock", () => {
    throw injected;
  });
  try {
    await t.test("injected child failure", async (child) => {
      await assert.rejects(
        runRetainLockFixture(child, { owned }),
        (error) => error === injected,
        "the injected probe error must win over publication error handling",
      );
    });
  } finally {
    probeInjections.delete("retainlock");
  }
  // The child finished; its registered teardown must have run. Teardown is
  // never invoked manually before these assertions.
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assert.equal(
    owned.services.length,
    0,
    "no services were acquired before the failure",
  );
  assertOwnedResourcesFinalized(owned);
});

test("permission-recovery fixture fails as a probe failure and cleans up automatically", async (t) => {
  const owned = {};
  const injected = new Error("injected probe failure");
  injected.code = "EIO";
  probeInjections.set("permission-recovery", () => {
    throw injected;
  });
  try {
    await t.test("injected child failure", async (child) => {
      await assert.rejects(
        runPermissionRecoveryFixture(child, { owned }),
        (error) => error === injected,
        "the injected probe error must win over publication error handling",
      );
    });
  } finally {
    probeInjections.delete("permission-recovery");
  }
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assertOwnedResourcesFinalized(owned);
});

test("an injected allowed probe outcome fails the guard and cleans up automatically", async (t) => {
  if (!isPosixNonRoot()) {
    t.skip("guard control requires a non-root POSIX environment");
    return;
  }
  const owned = {};
  probeInjections.set("retainlock", () => undefined);
  try {
    await t.test("injected allowed child failure", async (child) => {
      await assert.rejects(
        runRetainLockFixture(child, { owned }),
        /precondition failure/u,
        "the required-denial guard must fail the fixture",
      );
    });
  } finally {
    probeInjections.delete("retainlock");
  }
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assertOwnedResourcesFinalized(owned);
});

test("an injected allowed probe outcome fails the permission-recovery guard too", async (t) => {
  if (!isPosixNonRoot()) {
    t.skip("guard control requires a non-root POSIX environment");
    return;
  }
  const owned = {};
  probeInjections.set("permission-recovery", () => undefined);
  try {
    await t.test("injected allowed child failure", async (child) => {
      await assert.rejects(
        runPermissionRecoveryFixture(child, { owned }),
        /precondition failure/u,
        "the required-denial guard must fail the fixture",
      );
    });
  } finally {
    probeInjections.delete("permission-recovery");
  }
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assertOwnedResourcesFinalized(owned);
});

test("abort-cleanup probe failure cleans up the owned reservation automatically", async (t) => {
  const owned = {};
  const injected = new Error("injected probe failure");
  injected.code = "EIO";
  probeInjections.set("abort-cleanup", () => {
    throw injected;
  });
  try {
    await t.test("injected child failure", async (child) => {
      await assert.rejects(
        runAbortCleanupFixture(child, { owned }),
        (error) => error === injected,
        "the injected probe error must fail the fixture",
      );
    });
  } finally {
    probeInjections.delete("abort-cleanup");
  }
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assertOwnedResourcesFinalized(owned);
});

test("an injected allowed probe outcome fails the abort-cleanup guard and cleans up automatically", async (t) => {
  if (!isPosixNonRoot()) {
    t.skip("guard control requires a non-root POSIX environment");
    return;
  }
  const owned = {};
  // The injected attempt succeeds. The guard must reject this outcome in
  // a required non-root environment.
  probeInjections.set("abort-cleanup", () => undefined);
  try {
    await t.test("injected allowed child failure", async (child) => {
      await assert.rejects(
        runAbortCleanupFixture(child, { owned }),
        /precondition failure/u,
        "the required-denial guard must fail the fixture",
      );
    });
  } finally {
    probeInjections.delete("abort-cleanup");
  }
  // The child finished. Teardown ran automatically. We do not call it.
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assertOwnedResourcesFinalized(owned);
});

test("a setup failure right after allocation cleans up automatically", async (t) => {
  const owned = {};
  await t.test("injected setup failure", async (child) => {
    await assert.rejects(
      runAbortCleanupFixture(child, {
        owned,
        failDuringSetup: true,
      }),
      /injected setup failure/u,
    );
  });
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
});

test("a setup failure after reservation acquisition cleans up automatically", async (t) => {
  const owned = {};
  await t.test("injected setup failure", async (child) => {
    await assert.rejects(
      runAbortCleanupFixture(child, {
        owned,
        failAfterReservation: true,
      }),
      /injected setup failure/u,
    );
  });
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
  assertOwnedResourcesFinalized(owned);
});

test("a discovery service that stays open is closed by automatic teardown", async (t) => {
  // The discovery service exists only on the strict path, where mode bits
  // deny the unlink. On advisory platforms the fixture returns from its
  // advisory branch before the service is created.
  if (!isPosixNonRoot()) {
    t.skip("open-service control requires the strict denial path");
    return;
  }
  const owned = {};
  await t.test("injected child failure with a service open", async (child) => {
    await assert.rejects(
      runAbortCleanupFixture(child, {
        owned,
        failAfterDiscoveryService: true,
      }),
      /injected failure with the discovery service open/u,
    );
  });
  // The child finished. The service was open when teardown started. The
  // teardown must have closed it. We check this on the real method, by
  // identity: the operation log records the close call and its phase.
  assert.equal(owned.services.length, 2, "both services are tracked");
  assert.equal(
    owned.closedServices.size,
    owned.services.length,
    "every tracked service is closed",
  );
  const discovery = owned.services[1];
  const discoveryClose = (owned.operationLog ?? []).find(
    (entry) => entry.op === "close" && entry.resource === discovery && entry.ok,
  );
  assert.ok(
    discoveryClose,
    "the open discovery service is closed (observed on the real method)",
  );
  assert.equal(
    discoveryClose?.phase,
    "teardown",
    "the open discovery service is closed by teardown, not inline",
  );
  for (const service of owned.services) {
    assert.ok(owned.closedServices.has(service), "each service is closed");
  }
  assertOwnedResourcesFinalized(owned);
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
});

test("resource identities match the fixtures that created them", async (t) => {
  // Identity control: the abort-cleanup EIO failure tracks the parent
  // service and the primary reservation. The parent service is closed
  // inline before the injected probe error; the teardown finalizes the
  // reservation after the failure. Both operations are observed on the
  // real methods, by identity.
  const owned = {};
  const injected = new Error("injected probe failure");
  injected.code = "EIO";
  probeInjections.set("abort-cleanup", () => {
    throw injected;
  });
  try {
    await t.test("injected child failure", async (child) => {
      await assert.rejects(
        runAbortCleanupFixture(child, { owned }),
        (error) => error === injected,
      );
    });
  } finally {
    probeInjections.delete("abort-cleanup");
  }
  assert.equal(owned.services.length, 1, "the parent service is tracked");
  assert.equal(owned.reservations.length, 1, "the reservation is tracked");
  const log = owned.operationLog ?? [];
  const parentClose = log.find(
    (entry) =>
      entry.op === "close" && entry.resource === owned.services[0] && entry.ok,
  );
  assert.ok(
    parentClose,
    "the parent service is closed (observed on the real method)",
  );
  assert.equal(
    parentClose?.phase,
    "inline",
    "the parent service is closed inline, before the injected probe error",
  );
  const reservationAbort = log.find(
    (entry) =>
      entry.op === "abort" &&
      entry.resource === owned.reservations[0] &&
      entry.ok,
  );
  assert.ok(
    reservationAbort,
    "the reservation is finalized (observed on the real method)",
  );
  assert.equal(
    reservationAbort?.phase,
    "teardown",
    "the reservation is aborted by teardown, not inline",
  );
  const treeIndex = log.findIndex((entry) => entry.op === "tree");
  assert.ok(
    treeIndex !== -1 && log.indexOf(reservationAbort) < treeIndex,
    "the reservation is finalized before the tree removal",
  );
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
});

test("the retain-lock recovery reservation is aborted automatically after a later failure", async (t) => {
  // Recovery-acquisition control: the failure fires after the recovered
  // reservation exists and before it publishes, so it still holds the write
  // lock when the child fails. The child asserted the intentional retained
  // lock before the recovery stage; teardown must abort the recovered
  // reservation before the tree removal.
  if (!isPosixNonRoot()) {
    t.skip("recovery-reservation control requires the strict denial path");
    return;
  }
  const owned = {};
  await t.test("injected failure after recovery acquisition", async (child) => {
    await assert.rejects(
      runRetainLockFixture(child, {
        owned,
        failAfterRecoveredReservation: true,
      }),
      /injected failure after the recovered reservation/u,
    );
  });
  assertOwnedResourcesFinalized(owned);
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
});

test("the abort-cleanup recovery reservation is aborted automatically after a later failure", async (t) => {
  // Recovery-acquisition control: the failure fires after the recovered
  // reservation exists and before it publishes. The teardown must abort it
  // before the tree removal.
  if (!isPosixNonRoot()) {
    t.skip("recovery-reservation control requires the strict denial path");
    return;
  }
  const owned = {};
  await t.test("injected failure after recovery acquisition", async (child) => {
    await assert.rejects(
      runAbortCleanupFixture(child, {
        owned,
        failAfterRecoveredReservation: true,
      }),
      /injected failure after the recovered reservation/u,
    );
  });
  assertOwnedResourcesFinalized(owned);
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
});

test("the permission-recovery discovery service is closed automatically after a later failure", async (t) => {
  // Recovery-acquisition control: the failure fires after the discovery
  // service exists and before its inline close, so it is open when the
  // child fails. The teardown must close it before the tree removal.
  if (!isPosixNonRoot()) {
    t.skip("recovery-service control requires the strict denial path");
    return;
  }
  const owned = {};
  await t.test(
    "injected failure with the recovery service open",
    async (child) => {
      await assert.rejects(
        runPermissionRecoveryFixture(child, {
          owned,
          failAfterDiscoveryService: true,
        }),
        /injected failure with the recovery service open/u,
      );
    },
  );
  assertOwnedResourcesFinalized(owned);
  assert.ok(!existsSync(owned.tempDir), "the owned tree is removed");
});
