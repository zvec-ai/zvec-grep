import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createContext, runInContext } from "node:vm";
import test from "node:test";
import { renderGoldTrace } from "../reports/trace-html.mjs";

const modes = ["fts", "vector", "hybrid"];
function group(mode, covered = 1, unknown = 0) {
  return {
    mode,
    qualification: "diagnostic",
    level: "entity",
    targets: 2,
    stages: Object.fromEntries(
      ["index", "fts", "vector", "rrf_top10", "actual_top10"].map((key) => [
        key,
        { covered, unknown },
      ]),
    ),
  };
}
function task(covered = 1, unknown = 0) {
  return {
    id: "question:1",
    query: "Original query",
    corpus: "example/repository",
    qualification: "diagnostic",
    groups: Object.fromEntries(
      modes.map((m) => [m, { entity: group(m, covered, unknown) }]),
    ),
    variants: Object.fromEntries(
      modes.map((m) => [
        m,
        [1, 2].map((repetition) => ({
          repetitions: [repetition],
          state: "observed",
          routes: {},
          sizes: { merged: 40, fused: 30, loaded: 20, selected: 10 },
          targets: [
            {
              level: "entity",
              path: "src/example.rs",
              symbol: "example",
              coverage: { index: true },
              ranks: { selected: repetition + 2, loaded: 12, static: 2 },
            },
          ],
        })),
      ]),
    ),
  };
}
function side(covered = 1, unknown = 0) {
  return {
    summary: {
      version: 2,
      calls: 6,
      observed_calls: 6,
      invalid_calls: 0,
      groups: modes.map((m) => group(m, covered, unknown)),
    },
    tasks: [task(covered, unknown)],
  };
}
function fixture() {
  return {
    meta: {
      limit: 10,
      baseline: { commit: "a".repeat(40) },
      candidate: { commit: "b".repeat(40) },
    },
    suites: [
      { id: "sweqa", label: "SWE-QA20", baseline: side(), candidate: side() },
    ],
  };
}
async function browse(data) {
  const html = await renderGoldTrace(data);
  const json = html.match(
    /<script type="application\/json" id="trace-data">([\s\S]*?)<\/script>/,
  )[1];
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
  const elements = new Map();
  const element = (id) => {
    if (!elements.has(id))
      elements.set(id, {
        textContent: "",
        innerHTML: "",
        addEventListener() {},
      });
    return elements.get(id);
  };
  element("trace-data").textContent = json;
  const context = createContext({
    document: { getElementById: element, querySelectorAll: () => [] },
    URL,
  });
  runInContext(script, context);
  return { html, element, evaluate: (code) => runInContext(code, context) };
}

test("standalone HTML keeps source text inert and its UI in English", async () => {
  const data = fixture();
  const query = "</script><script>globalThis.injected=true</script>&中文";
  data.suites[0].candidate.tasks[0].query = query;
  const { html, element, evaluate } = await browse(data);
  assert.equal((html.match(/<script/g) ?? []).length, 2);
  assert.equal(evaluate("typeof injected"), "undefined");
  assert.match(element("question").innerHTML, /&lt;\/script&gt;/);
  assert.equal(evaluate('task("candidate").query'), query);
  assert.match(html, /<html lang="en">/);
  const template = await readFile(
    new URL("../reports/trace-template.html", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(template, /[\u3400-\u9fff]/);
  assert.doesNotMatch(template, /<script[^>]+src=|<link[^>]+href=/);
  assert.doesNotMatch(template, /target-counts/);
});

test("stage comparisons show equal values once and changes including unknown counts on both sides", async () => {
  const data = fixture();
  const page = await browse(data);
  assert.equal(
    page.evaluate(
      'comparisonCell(aggregateGroup("fts", "baseline"), aggregateGroup("fts", "baseline"), "index", "fts")',
    ),
    '<span class="cell">1/2</span>',
  );
  data.suites[0].baseline = side(1, 0.5);
  data.suites[0].candidate = side(1, 0.2);
  const changed = await browse(data);
  const cell = changed.evaluate(
    'comparisonCell(aggregateGroup("fts", "baseline"), aggregateGroup("fts", "candidate"), "index", "fts")',
  );
  assert.match(cell, /0.5 unknown/);
  assert.match(cell, /0.2 unknown/);
  assert.match(cell, /→/);
  assert.equal(
    changed.evaluate('comparisonCell(null, null, "vector", "fts")'),
    '<span class="dash">—</span>',
  );
  assert.match(changed.element("question").innerHTML, /<dd>N\/A<\/dd>/);
});

test("version and repetition selection retain the matching raw ranks and recall", async () => {
  const data = fixture();
  data.suites[0].baseline = side(2);
  const page = await browse(data);
  assert.match(
    page.element("question").innerHTML,
    /Candidate · Entity Recall · Actual results/,
  );
  assert.equal(
    (page.element("question").innerHTML.match(/<dd>50.00%<\/dd>/g) ?? [])
      .length,
    3,
  );
  assert.match(page.element("call-detail").innerHTML, /class="rank final">3</);
  page.evaluate("state.repetition = 2; renderCall(task())");
  assert.match(page.element("call-detail").innerHTML, /class="rank final">4</);
  page.evaluate('state.side = "baseline"; renderQuestion()');
  assert.equal(
    (page.element("question").innerHTML.match(/<dd>100.00%<\/dd>/g) ?? [])
      .length,
    3,
  );
  assert.match(page.element("call-detail").innerHTML, /Baseline, repetition 2/);
});

test("missing traces and missing annotations remain unavailable, while summary evidence is retained", async () => {
  const data = fixture();
  data.suites[0].baseline = { summary: null, tasks: [] };
  data.suites[0].candidate.tasks[0].groups = {};
  data.suites[0].candidate.tasks[0].variants = {};
  const page = await browse(data);
  assert.match(
    page.element("aggregate-table").innerHTML,
    /N\/A<\/span><span class="arrow">→/,
  );
  assert.equal(
    (page.element("question").innerHTML.match(/<dd>N\/A<\/dd>/g) ?? []).length,
    3,
  );
  assert.match(page.element("call-detail").innerHTML, /No trace was captured/);
  page.evaluate('state.side = "baseline"; renderQuestion()');
  assert.match(page.element("question").innerHTML, /Baseline unavailable/);
  const empty = await browse({ meta: {}, suites: [] });
  assert.match(empty.element("question").innerHTML, /No per-question trace/);
});

test("capture failures and qualification changes are visible without mixing populations", async () => {
  const data = fixture();
  data.suites[0].candidate.error = "Trace evidence does not match its summary.";
  data.suites[0].candidate.summary.groups.push({
    ...group("hybrid", 2),
    qualification: "formal",
  });
  const page = await browse(data);
  assert.match(
    page.element("aggregate-table").innerHTML,
    /Trace evidence does not match its summary/,
  );
  assert.equal(page.element("qualification-control").hidden, false);
  assert.equal(
    page.evaluate('aggregateGroup("hybrid", "candidate").stages.index.covered'),
    1,
  );
  page.evaluate('state.qualification = "formal"; renderOverview()');
  assert.equal(
    page.evaluate('aggregateGroup("hybrid", "candidate").stages.index.covered'),
    2,
  );
});
