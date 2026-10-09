import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  endsWithRelative,
  physicallyUnder,
  toPortableSeparators,
} from "../helpers/native-path.mjs";

// Hosted regression (CI run 36778689252, Windows): expected-file and
// deletion assertions compared native-separator hit paths against
// forward-slash suffixes and never matched. These controls pin the
// platform-aware behavior; the string-level old-style control is
// archived alongside the run logs.
test("endsWithRelative matches expected files across native separators", () => {
  assert.equal(
    endsWithRelative("C:\\ws\\B\\notes\\plain.txt", "notes/plain.txt"),
    true,
  );
  assert.equal(
    endsWithRelative("/tmp/x/B/notes/plain.txt", "notes/plain.txt"),
    true,
  );
  assert.equal(endsWithRelative("notes/plain.txt", "notes/plain.txt"), true);
});

test("endsWithRelative stays segment-aligned and discriminating", () => {
  assert.equal(
    endsWithRelative("C:\\ws\\B\\Xnotes\\plain.txt", "notes/plain.txt"),
    false,
  );
  assert.equal(
    endsWithRelative("C:\\ws\\B\\notes\\plain.txt", "docs/renamed.md"),
    false,
  );
  assert.equal(endsWithRelative("C:\\ws\\src\\util.ts", "src/util.ts"), true);
  assert.equal(
    endsWithRelative("C:\\ws\\docs\\renamed.md", "src/util.ts"),
    false,
  );
});

test("toPortableSeparators normalizes both separator styles on every platform", () => {
  assert.equal(toPortableSeparators("a/b\\c.md"), "a/b/c.md");
  assert.equal(toPortableSeparators("a\\b\\c.md"), "a/b/c.md");
  assert.equal(toPortableSeparators("a/b/c.md"), "a/b/c.md");
});

test("physicallyUnder resolves both sides through a workspace alias", async (t) => {
  const base = await mkdtemp(join(tmpdir(), "native-path-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const physical = join(base, "real", "A");
  const elsewhere = join(base, "real", "B");
  await mkdir(join(physical, "docs"), { recursive: true });
  await mkdir(elsewhere, { recursive: true });
  const file = join(physical, "docs", "one.md");
  await writeFile(file, "# One\n");
  await symlink(join(base, "real"), join(base, "var"));

  assert.equal(physicallyUnder(file, join(base, "var", "A")), true);
  assert.equal(physicallyUnder(file, physical), true);
  assert.equal(physicallyUnder(file, elsewhere), false);
});
