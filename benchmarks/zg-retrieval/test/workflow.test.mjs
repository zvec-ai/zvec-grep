import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const workflow = await readFile(
  new URL(".github/workflows/retrieval-only.yml", repository),
  "utf8",
);
const prWorkflow = await readFile(
  new URL(".github/workflows/retrieval-pr.yml", repository),
  "utf8",
);
const action = await readFile(
  new URL(".github/actions/retrieval-authorize/action.yml", repository),
  "utf8",
);
const comparisonWorkflow = await readFile(
  new URL(".github/workflows/retrieval-comparison.yml", repository),
  "utf8",
);
const rustAction = await readFile(
  new URL(".github/actions/rust-candidate-package/action.yml", repository),
  "utf8",
);

// These bounded layout readers check our checked-in workflow contract. They are
// not YAML parsers; actionlint validates the complete workflow/action syntax.
function block(source, key, indent) {
  const lines = source.split("\n");
  const start = lines.findIndex(
    (line) => line === `${" ".repeat(indent)}${key}:`,
  );
  assert.notEqual(start, -1, `missing ${key} block`);
  const result = [];
  for (const line of lines.slice(start + 1)) {
    if (line.trim() && line.search(/\S/) <= indent) break;
    result.push(line);
  }
  return result.join("\n");
}

const jobs = Object.fromEntries(
  [...block(workflow, "jobs", 0).matchAll(/^ {2}([\w-]+):$/gm)].map((match) => [
    match[1],
    block(workflow, match[1], 2),
  ]),
);
function steps(job) {
  const starts = [...job.matchAll(/^ {6}- \S.*$/gm)];
  return starts.map((match, index) =>
    job.slice(match.index, starts[index + 1]?.index ?? job.length),
  );
}

test("Retrieval-only retains its reusable and manual candidate/embedding inputs", async () => {
  assert.deepEqual(
    [...block(workflow, "on", 0).matchAll(/^ {2}([\w-]+):$/gm)].map(
      (match) => match[1],
    ),
    ["workflow_call", "workflow_dispatch"],
  );
  const dispatch = block(workflow, "workflow_dispatch", 2);
  assert.match(dispatch, /^ {4}inputs:$/m);
  assert.match(dispatch, /^ {6}candidate_ref:$/m);
  assert.match(dispatch, /^ {8}default: main$/m);
  assert.match(dispatch, /^ {6}embedding:$/m);
  assert.match(dispatch, /^ {8}default: local$/m);
  assert.match(dispatch, /^ {8}type: choice$/m);
  assert.match(dispatch, /^ {10}- local$/m);
  assert.match(dispatch, /^ {10}- remote$/m);
  assert.equal((dispatch.match(/^ {6}[a-z_]+:$/gm) ?? []).length, 2);
  const files = await readdir(new URL(".github/workflows/", repository));
  assert.deepEqual(
    files.filter((file) => /^retrieval.*\.ya?ml$/.test(file)).sort(),
    [
      "retrieval-comparison.yml",
      "retrieval-only.yml",
      "retrieval-pr-comment.yml",
      "retrieval-pr.yml",
    ],
  );
  assert.deepEqual(Object.keys(jobs), [
    "authorize",
    "quality-contract",
    "package-candidate",
    "sweqa",
    "beir",
    "duretrieval",
    "quarry",
    "results",
  ]);
  assert.doesNotMatch(workflow, /setup-node|node-version|NODE_VERSION/);
});

test("PR updates run Retrieval-only at the fork head SHA with local embedding and no forwarded secrets", () => {
  assert.deepEqual(
    [...block(prWorkflow, "on", 0).matchAll(/^ {2}([\w-]+):$/gm)].map(
      (match) => match[1],
    ),
    ["pull_request", "workflow_dispatch"],
  );
  assert.match(
    block(prWorkflow, "pull_request", 2),
    /types: \[opened, synchronize, reopened\]/,
  );
  assert.match(prWorkflow, /^permissions: \{\}$/m);
  const job = block(prWorkflow, "retrieval", 2);
  assert.equal(
    [...block(prWorkflow, "jobs", 0).matchAll(/^ {2}([\w-]+):$/gm)].length,
    2,
  );
  assert.match(job, /uses: \.\/\.github\/workflows\/retrieval-comparison\.yml/);
  assert.equal(block(prWorkflow, "permissions", 4).trim(), "contents: read");
  assert.match(
    job,
    /candidate_ref: \$\{\{ inputs\.candidate_sha \|\| github\.event\.pull_request\.head\.sha \}\}/,
  );
  assert.match(
    job,
    /candidate_repository: \$\{\{ inputs\.candidate_repository \|\| github\.event\.pull_request\.head\.repo\.full_name \}\}/,
  );
  assert.match(
    job,
    /baseline_ref: \$\{\{ inputs\.baseline_sha \|\| github\.event\.pull_request\.base\.sha \}\}/,
  );
  assert.match(
    job,
    /baseline_repository: \$\{\{ inputs\.baseline_repository \|\| github\.event\.pull_request\.base\.repo\.full_name \}\}/,
  );
  assert.doesNotMatch(
    prWorkflow,
    /secrets:|secrets\.|inherit|: write|swe-qa-bench\.yml/,
  );
  assert.match(
    block(workflow, "env", 0),
    /RETRIEVAL_EMBEDDING: \$\{\{ inputs\.embedding \}\}/,
  );
});

test("automatic PR concurrency cancels older commits without cancelling manual benchmark runs", () => {
  const concurrency = block(prWorkflow, "concurrency", 0);
  assert.match(
    concurrency,
    /group: retrieval-pr-\$\{\{ github\.event\.pull_request\.number \|\| github\.run_id \}\}/,
  );
  assert.match(concurrency, /cancel-in-progress: true/);
  assert.doesNotMatch(concurrency, /head\.sha/);
  assert.match(block(workflow, "concurrency", 0), /group: retrieval-only-/);
});

test("version comparison keeps artifact namespaces separate and uses local models without secrets", () => {
  for (const [job, prefix] of [
    ["baseline", "retrieval-base"],
    ["candidate", "retrieval-head"],
  ]) {
    const source = block(comparisonWorkflow, job, 2);
    assert.match(source, /uses: \.\/\.github\/workflows\/retrieval-only\.yml/);
    assert.match(source, /embedding: local/);
    assert.ok(source.includes(`artifact_prefix: ${prefix}`));
  }
  assert.doesNotMatch(
    comparisonWorkflow,
    /secrets:|secrets\.|: write|swe-qa-bench/,
  );
  const compare = block(comparisonWorkflow, "compare", 2);
  assert.match(compare, /if: always\(\) && !cancelled\(\)/);
  assert.match(compare, /name: retrieval-base-results/);
  assert.match(compare, /name: retrieval-head-results/);
  assert.match(compare, /name: retrieval-comparison/);
  assert.match(workflow, /retrieval-results\/reports\//);
});

test("local embedding is the default and remote Qwen credentials stay scoped to execution", () => {
  assert.match(jobs.authorize, /Check remote embedding configuration/);
  assert.match(jobs.authorize, /if: inputs\.embedding == 'remote'/);
  assert.match(jobs.authorize, /secrets\.QWEN_EMBEDDING_API_KEY/);
  assert.match(jobs.authorize, /vars\.QWEN_EMBEDDING_ENDPOINT/);
  for (const suite of ["sweqa", "beir", "duretrieval", "quarry"]) {
    assert.match(
      jobs[suite],
      /RETRIEVAL_EMBEDDING: \$\{\{ inputs\.embedding \}\}/,
    );
    assert.match(
      jobs[suite],
      /if: inputs\.embedding == 'local'[\s\S]*uses: actions\/cache@/,
      `${suite}: local model cache`,
    );
    const run = steps(jobs[suite]).find((entry) =>
      /node benchmarks\/zg-retrieval\/(?:expansion\/)?run\.mjs/.test(entry),
    );
    assert.ok(run, suite);
    assert.match(run, /secrets\.QWEN_EMBEDDING_API_KEY/);
    assert.match(run, /vars\.QWEN_EMBEDDING_ENDPOINT/);
  }
  assert.match(
    jobs.results,
    /RETRIEVAL_EMBEDDING: \$\{\{ inputs\.embedding \}\}/,
  );
  assert.doesNotMatch(jobs["quality-contract"], /RETRIEVAL_EMBEDDING/);
});

test("the four suites run in independent jobs using one candidate package", async () => {
  const protocol = JSON.parse(
    await readFile(
      new URL("benchmarks/zg-retrieval/configs/protocol.json", repository),
      "utf8",
    ),
  );
  assert.deepEqual(protocol.modes, ["hybrid", "fts", "vector"]);
  assert.equal(protocol.preview, "mcp-default");
  const lock = JSON.parse(
    await readFile(
      new URL("benchmarks/zg-retrieval/data/source.lock.json", repository),
      "utf8",
    ),
  );
  assert.equal(lock.repositories.length, 11);
  assert.equal(
    new Set(lock.repositories.map((repo) => repo.repository)).size,
    11,
  );
  assert.equal(Object.keys(jobs).length, 8);
  assert.doesNotMatch(workflow, /strategy:|fromJSON\(/);
  const runner = steps(jobs.sweqa).find((entry) =>
    entry.includes("node benchmarks/zg-retrieval/run.mjs"),
  );
  assert.ok(runner);
  assert.doesNotMatch(runner, /--(?:modes|preview)\b|RETRIEVAL_MODES|inputs\./);
  assert.doesNotMatch(runner, /--repository\b/);
  assert.match(jobs["quality-contract"], /uses: actions\/setup-python@/);
  assert.match(jobs.sweqa, /needs: \[authorize, package-candidate\]/);
  assert.match(jobs.sweqa, /ci-report\.mjs/);
  assert.match(jobs.sweqa, /name:.*-zg-report/);
  assert.match(jobs.sweqa, /\$GITHUB_STEP_SUMMARY/);
  for (const suite of ["beir", "duretrieval", "quarry"]) {
    assert.match(jobs[suite], /needs: \[authorize, package-candidate\]/);
    assert.match(jobs[suite], new RegExp(`--suite ${suite}`));
    assert.match(jobs[suite], new RegExp(`name:.*-${suite}-report`));
    assert.match(jobs[suite], /\$GITHUB_STEP_SUMMARY/);
  }
  assert.match(
    jobs["quality-contract"],
    /node --test benchmarks\/zg-retrieval\/test\/\*\.test\.mjs/,
  );
  assert.match(jobs.duretrieval, /pyarrow|requirements-duretrieval\.txt/);
  assert.match(jobs.beir, /requirements-beir\.txt/);
  assert.doesNotMatch(jobs.quarry, /pip install|python -m venv|SDK parity/);
});

test("the selected workflow ref is frozen once for every downstream job", () => {
  const authorizeCheckout = steps(jobs.authorize)[0];
  assert.doesNotMatch(authorizeCheckout, /^ {10}ref:/m);
  assert.match(
    jobs.authorize,
    /harness-commit: \$\{\{ steps\.harness\.outputs\.commit \}\}/,
  );
  for (const name of [
    "quality-contract",
    "package-candidate",
    "sweqa",
    "beir",
    "duretrieval",
    "quarry",
    "results",
  ])
    assert.match(
      jobs[name],
      /ref: \$\{\{ needs\.authorize\.outputs\.harness-commit \}\}/,
      name,
    );
});

test("the selected source is built from rust/ and exact-commit package caching bypasses recompilation", () => {
  const job = jobs["package-candidate"];
  assert.match(job, /uses: \.\/\.github\/actions\/rust-candidate-package/);
  assert.match(
    job,
    /candidate_repository: \$\{\{ inputs\.candidate_repository \|\| \(inputs\.candidate_ref == 'main' && 'zvec-ai\/zvec-grep' \|\| github\.repository\) \}\}/,
  );
  assert.match(job, /candidate_ref: \$\{\{ inputs\.candidate_ref \}\}/);
  assert.match(job, /cache_namespace: retrieval-rust/);
  assert.match(
    job,
    /candidate-commit: \$\{\{ steps\.rust-candidate\.outputs\.commit \}\}/,
  );
  assert.match(
    rustAction,
    /repository: \$\{\{ inputs\.candidate_repository \}\}/,
  );
  assert.match(rustAction, /ref: \$\{\{ inputs\.candidate_ref \}\}/);
  assert.match(rustAction, /path: candidate/);
  assert.match(rustAction, /working-directory: candidate\/rust/);
  // PR-scoped Cargo caches are never restored by other PRs, so only the
  // commit-bound package is cached.
  assert.doesNotMatch(rustAction, /candidate\/rust\/target\//);
  assert.match(
    rustAction,
    /\$\{\{ inputs\.cache_namespace \}\}-package-v1-\$\{\{ runner\.os \}\}-\$\{\{ runner\.arch \}\}-\$\{\{ steps\.source\.outputs\.commit \}\}/,
  );
  assert.match(
    rustAction,
    /if: steps\.rust-package-cache\.outputs\.cache-hit != 'true'/,
  );
  assert.match(rustAction, /npm run pack:local/);
  assert.match(rustAction, /--source \.\./);
  assert.doesNotMatch(
    job,
    /node-version[^\n]*\$\{\{|NODE_VERSION|node.*cache.*key/i,
  );
  for (const suite of ["sweqa", "beir", "duretrieval", "quarry"]) {
    assert.match(jobs[suite], /Verify the candidate package identity/);
    assert.match(
      jobs[suite],
      /--commit "\$\{\{ needs\.package-candidate\.outputs\.candidate-commit \}\}"/,
    );
  }
});

test("every independently rerunnable job checks benchmark access before doing work", () => {
  assert.equal(Object.keys(jobs).length, 8);
  assert.ok(jobs.results && jobs.sweqa && jobs.authorize);
  for (const [name, job] of Object.entries(jobs)) {
    const entries = steps(job);
    assert.match(entries[0], /uses: actions\/checkout@/, `${name}: checkout`);
    assert.match(
      entries[1],
      /uses: \.\/\.github\/actions\/retrieval-authorize/,
      `${name}: authorization must precede setup, installation and execution`,
    );
    assert.match(entries[1], /^ {8}id: access$/m, `${name}: access step ID`);
    assert.doesNotMatch(entries[1], /continue-on-error|\bif:/);
    assert.equal(
      entries.filter((entry) =>
        entry.includes("uses: ./.github/actions/retrieval-authorize"),
      ).length,
      1,
      `${name}: exactly one authorization gate`,
    );
    for (const entry of entries.filter((entry) =>
      /if:.*always\(\)/.test(entry),
    ))
      assert.match(
        entry,
        /if:.*steps\.access\.outcome == 'success'/,
        `${name}: always() must not bypass denied permission`,
      );
  }
  assert.match(action, /DISPATCH_ACTOR: \$\{\{ github\.actor \}\}/);
  assert.match(action, /RERUN_ACTOR: \$\{\{ github\.triggering_actor \}\}/);
});

test("one final page combines all suite reports after successful or failed jobs", () => {
  assert.match(jobs.results, /^ {4}if:.*always\(\)/m);
  const dependencies = /needs:\s*\[([\s\S]*?)\]/.exec(jobs.results);
  assert.ok(dependencies);
  assert.deepEqual(
    dependencies[1]
      .split(",")
      .map((name) => name.trim())
      .filter(Boolean),
    [
      "authorize",
      "quality-contract",
      "package-candidate",
      "sweqa",
      "beir",
      "duretrieval",
      "quarry",
    ],
  );
  const finalSteps = steps(jobs.results);
  const downloads = finalSteps.filter((entry) =>
    entry.includes("uses: actions/download-artifact@"),
  );
  assert.equal(downloads.length, 4);
  assert.match(downloads[0], /name:.*-zg-report/);
  assert.match(downloads[0], /continue-on-error: true/);
  assert.match(downloads[1], /name:.*-beir-report/);
  assert.match(downloads[2], /name:.*-duretrieval-report/);
  assert.match(downloads[3], /name:.*-quarry-report/);
  const builder = finalSteps.find((entry) =>
    entry.includes("expansion/combined.mjs"),
  );
  assert.ok(builder);
  assert.match(builder, /if:.*always\(\)/);
  assert.match(builder, /RETRIEVAL_JOB_RESULTS: \$\{\{ toJSON\(needs\) \}\}/);
  assert.deepEqual(
    [...builder.matchAll(/^\s+--([\w-]+)/gm)].map((match) => match[1]),
    ["zg", "beir", "duretrieval", "quarry", "output"],
  );
  assert.doesNotMatch(builder, /comparison|baseline|preview|modes/);
  const overviewArtifact = finalSteps.find(
    (entry) =>
      entry.includes("-results") &&
      entry.includes("uses: actions/upload-artifact@"),
  );
  assert.ok(overviewArtifact);
  assert.match(overviewArtifact, /retrieval-results\/summary\.md/);
  assert.match(overviewArtifact, /retrieval-results\/summary\.json/);
  assert.doesNotMatch(overviewArtifact, /comparison/);
  const publishers = Object.entries(jobs).flatMap(([name, job]) =>
    steps(job)
      .filter((entry) => entry.includes("$GITHUB_STEP_SUMMARY"))
      .map((entry) => ({ name, entry })),
  );
  assert.equal(publishers.length, 5);
  assert.deepEqual(
    publishers.map((entry) => entry.name),
    ["sweqa", "beir", "duretrieval", "quarry", "results"],
  );
  for (const publisher of publishers)
    assert.match(publisher.entry, /if:.*always\(\)/);
  assert.match(publishers[0].entry, /summary\.md/);
  assert.match(publishers[4].entry, /summary\.md/);
  assert.match(publishers[4].entry, /missing results are not zero scores/);
});

const scriptStart = action.indexOf("        script: |\n");
assert.notEqual(scriptStart, -1);
const script = action
  .slice(scriptStart + "        script: |\n".length)
  .split("\n")
  .filter((line) => line.trim())
  .map((line) => {
    assert.ok(
      line.startsWith("          "),
      "unexpected authorization script indentation",
    );
    return line.slice(10);
  })
  .join("\n");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const authorize = new AsyncFunction(
  "github",
  "core",
  "context",
  "process",
  script,
);
const maintain = { permission: "write", role_name: "maintain" };
const admin = { permission: "admin", role_name: "admin" };

function attempt({
  eventName = "workflow_dispatch",
  dispatch = "maintainer",
  rerun = dispatch,
  roles = { maintainer: maintain },
  apiError,
  payload = {},
  embedding = "local",
} = {}) {
  const calls = [],
    messages = [],
    summaryCalls = [];
  const summary = {
    addHeading(value) {
      summaryCalls.push(["heading", value]);
      return this;
    },
    addRaw(value) {
      summaryCalls.push(["raw", value]);
      return this;
    },
    async write() {
      summaryCalls.push(["write"]);
    },
  };
  const github = {
    rest: {
      repos: {
        async getCollaboratorPermissionLevel(input) {
          calls.push(input);
          assert.deepEqual(
            { owner: input.owner, repo: input.repo },
            { owner: "owner", repo: "repo" },
          );
          if (apiError) throw apiError;
          return {
            data: roles[input.username] ?? {
              permission: "none",
              role_name: "none",
            },
          };
        },
      },
    },
  };
  return {
    calls,
    messages,
    summaryCalls,
    run: () =>
      authorize(
        github,
        { info: (message) => messages.push(message), summary },
        { eventName, payload, repo: { owner: "owner", repo: "repo" } },
        {
          env: {
            DISPATCH_ACTOR: dispatch,
            RERUN_ACTOR: rerun,
            RETRIEVAL_EMBEDDING: embedding,
          },
        },
      ),
  };
}

function assertDeniedSummary(result) {
  assert.equal(
    result.summaryCalls.filter(([type]) => type === "write").length,
    1,
  );
  assert.deepEqual(result.summaryCalls[0], [
    "heading",
    "Retrieval-only: access denied",
  ]);
  assert.match(
    result.summaryCalls.find(([type]) => type === "raw")[1],
    /No results were produced by this job/,
  );
}

test("the embedded authorization script permits maintain and admin and de-duplicates the same actor", async () => {
  for (const role of [maintain, admin]) {
    const result = attempt({ roles: { maintainer: role } });
    await result.run();
    assert.equal(result.calls.length, 1);
    assert.equal(result.messages.length, 1);
    assert.deepEqual(result.summaryCalls, []);
  }
  const result = attempt({
    rerun: "administrator",
    roles: { maintainer: maintain, administrator: admin },
  });
  await result.run();
  assert.deepEqual(
    result.calls.map((call) => call.username),
    ["maintainer", "administrator"],
  );
});

for (const role of ["write", "triage", "read", "none"]) {
  test(`the embedded authorization script denies ${role} access and writes a denial summary`, async () => {
    const result = attempt({
      roles: {
        maintainer: {
          permission: role === "triage" ? "read" : role,
          role_name: role,
        },
      },
    });
    await assert.rejects(
      result.run(),
      /requires the maintain or admin repository role/,
    );
    assertDeniedSummary(result);
  });
}

test("a successful original maintainer cannot authorize a write-only actor's partial re-run", async () => {
  const result = attempt({
    rerun: "writer",
    roles: {
      maintainer: maintain,
      writer: { permission: "write", role_name: "write" },
    },
  });
  await assert.rejects(
    result.run(),
    /writer requires the maintain or admin repository role/,
  );
  assert.deepEqual(
    result.calls.map((call) => call.username),
    ["maintainer", "writer"],
  );
  assertDeniedSummary(result);
});

test("missing actors, nonmanual events and permission API failures are denied rather than allowed", async () => {
  for (const options of [{ dispatch: "" }, { rerun: "" }]) {
    const result = attempt(options);
    await assert.rejects(result.run(), /Missing workflow actor/);
    assertDeniedSummary(result);
  }
  for (const eventName of ["push", "workflow_run", "pull_request_target"]) {
    const result = attempt({ eventName });
    await assert.rejects(
      result.run(),
      /pull_request, workflow_dispatch or a PR comment caller/,
    );
    assert.equal(result.calls.length, 0);
    assertDeniedSummary(result);
  }
  const apiError = new Error("permission API unavailable");
  const result = attempt({ apiError });
  await assert.rejects(result.run(), (error) => error === apiError);
  assertDeniedSummary(result);
});

test("automatic local PR runs and partial reruns do not require collaborator permissions", async () => {
  for (const action of ["opened", "synchronize", "reopened"]) {
    const result = attempt({
      eventName: "pull_request",
      dispatch: "contributor",
      rerun: "writer",
      apiError: new Error("fork token cannot read collaborator permissions"),
      payload: { action, pull_request: { head: { sha: "a".repeat(40) } } },
    });
    await result.run();
    assert.equal(result.calls.length, 0);
    assert.match(
      result.messages[0],
      /automatic PR retrieval with local embedding/,
    );
    assert.deepEqual(result.summaryCalls, []);
  }
});

test("PR authorization rejects remote or missing embedding and unsupported PR events", async () => {
  for (const embedding of ["remote", "", undefined]) {
    const result = attempt({
      eventName: "pull_request",
      embedding: embedding ?? "",
      payload: { action: "opened", pull_request: {} },
    });
    await assert.rejects(result.run(), /requires local embedding/);
    assert.equal(result.calls.length, 0);
    assertDeniedSummary(result);
  }
  for (const payload of [
    {},
    { action: "opened" },
    { action: "edited", pull_request: {} },
    { action: "closed", pull_request: {} },
  ]) {
    const result = attempt({ eventName: "pull_request", payload });
    await assert.rejects(result.run(), /Unsupported pull_request event/);
    assert.equal(result.calls.length, 0);
    assertDeniedSummary(result);
  }
});

test("reusable retrieval accepts created PR comments with maintainer permission", async () => {
  const result = attempt({
    eventName: "issue_comment",
    payload: { action: "created", issue: { pull_request: {} } },
  });
  await result.run();
  assert.equal(result.calls.length, 1);
  for (const payload of [
    { action: "created", issue: {} },
    { action: "edited", issue: { pull_request: {} } },
  ]) {
    const denied = attempt({ eventName: "issue_comment", payload });
    await assert.rejects(denied.run(), /PR comment caller/);
    assert.equal(denied.calls.length, 0);
  }
});
