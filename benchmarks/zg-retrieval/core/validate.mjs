import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";
import {
  loadSuite,
  repositorySlug,
  run,
  validateGoldSources,
  validateQaGoldSources,
} from "./lib.mjs";

/** Validate frozen inputs and, optionally, existing source checkouts. Never fetch or build an index. */
export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: {
      corpus: { type: "string" },
      repository: { type: "string" },
    },
  });
  assert.ok(
    !values.repository || values.corpus,
    "--repository requires --corpus",
  );
  const suite = await loadSuite();
  const entries = Object.values(suite.gold);
  const checked = [];
  if (values.corpus) {
    const repositories = suite.lock.repositories.filter(
      (repo) => !values.repository || repo.repository === values.repository,
    );
    assert.ok(repositories.length, `unknown repository: ${values.repository}`);
    for (const repo of repositories) {
      const root = join(
        resolve(values.corpus),
        repositorySlug(repo.repository),
      );
      const head = (
        await run("git", ["-C", root, "rev-parse", "HEAD"])
      ).stdout.trim();
      assert.equal(
        head,
        repo.commit,
        `${repo.repository}: source commit mismatch`,
      );
      const dirty = await run("git", [
        "-C",
        root,
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--",
        ".",
        ":(exclude).zvec-grep",
      ]);
      assert.equal(
        dirty.stdout,
        "",
        `${repo.repository}: source checkout is not pristine`,
      );
      const tasks = suite.lock.tasks.filter(
        (task) => task.repository === repo.repository,
      );
      await validateGoldSources(root, tasks, suite.gold);
      await validateQaGoldSources(root, tasks, suite.qa_pairs);
      checked.push({
        repository: repo.repository,
        commit: head,
        tasks: tasks.map((task) => task.task_id),
      });
    }
  }
  const result = {
    valid: true,
    suite: suite.identity,
    tasks: suite.lock.tasks.length,
    repositories: suite.lock.repositories.length,
    reviewed_tasks: entries.filter((entry) => entry.status === "reviewed")
      .length,
    targets: entries.reduce((sum, entry) => sum + entry.targets.length, 0),
    ndcg_tasks: entries.filter((entry) => entry.ndcg.enabled).length,
    qa_gold: suite.qa_gold,
    qa_formal_tasks: Object.values(suite.qa_pairs).filter(
      (entry) => entry.scoring_eligible,
    ).length,
    qa_diagnostic_tasks: Object.values(suite.qa_pairs).filter(
      (entry) => entry.diagnostic_eligible,
    ).length,
    source_validation: values.corpus ? "verified" : "not_requested",
    checked_sources: checked,
  };
  console.log(JSON.stringify(result, null, 2));
  return result;
}
