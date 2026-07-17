import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("zvec-grep skill triggers by task and selects the available transport", async () => {
  const skill = await readFile("skills/zvec-grep/SKILL.md", "utf8");
  const metadata = await readFile(
    "skills/zvec-grep/agents/openai.yaml",
    "utf8",
  );
  const fallback = await readFile(
    "skills/zvec-grep/references/cli-fallback.md",
    "utf8",
  );

  assert.match(
    skill,
    /^description: Repository code search and indexing with zvec-grep/m,
  );
  assert.match(
    skill,
    /repository investigation would otherwise use grep, rg, or broad file reads/,
  );
  assert.match(skill, /Use zvec-grep before raw `grep` or `rg`/);
  assert.match(
    skill,
    /Use native HTTP MCP tools as the primary interface when the matching `zvec_grep_\*` tool is present/,
  );
  assert.match(skill, /required MCP tool is absent from the current task/);
  assert.match(skill, /default Auto mode can select Server or Direct/);
  assert.match(
    skill,
    /do not probe forced Server mode and then retry forced Direct mode/,
  );
  assert.match(skill, /`wait` parameter defaults to false/i);
  assert.match(skill, /Poll `zvec_grep_index_status` only when completion/);
  assert.match(skill, /server default is known; never guess a model/);
  assert.match(skill, /zvec_grep_index_status/);
  assert.match(skill, /Call `zvec_grep_search` first/);
  assert.match(skill, /Call `zvec_grep_rg` for exhaustive local ripgrep/);
  assert.match(skill, /Its `drop` parameter deletes the workspace index/);
  assert.match(skill, /`freshness` and `indexing`/);
  assert.doesNotMatch(skill, /Call `zvec_grep_index_status` once at the start/);
  assert.match(skill, /references\/cli-fallback\.md/);
  assert.doesNotMatch(skill, /Use zvec-grep through the `zg` command/);
  assert.match(metadata, /Repository search, indexing, and daemon diagnostics/);
  assert.match(
    metadata,
    /Use \$zvec-grep to investigate this repository before raw grep or rg/,
  );
  assert.match(fallback, /Leave `--mode` unset/);
  assert.match(fallback, /zg status\r?\n/);
  assert.doesNotMatch(fallback, /zg status --mode (?:server|direct)/);
  assert.doesNotMatch(fallback, /zg query[^\n]*--mode (?:server|direct)/);
  assert.match(fallback, /zg query "request validation"/);
  assert.match(fallback, /already attempts CPU fallback/);
  assert.match(
    fallback,
    /Do not repeat the query with an explicit CPU override/,
  );
  assert.doesNotMatch(fallback, /--no-gpu --embedding-parallelism 1/);
  assert.match(
    fallback,
    /embedding context remains unavailable and exact anchors are available[\s\S]*existing indexed FTS route/,
  );
  assert.match(
    fallback,
    /Do not switch to managed ripgrep merely because semantic search is unavailable/,
  );
  assert.match(fallback, /server default model is known/);
  assert.match(fallback, /zg index\r?\n/);
  assert.doesNotMatch(fallback, /zg index[^\n]*--mode (?:server|direct)/);
});
