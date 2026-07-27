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
    /Use the public native HTTP MCP search tools as the primary interface when the matching `zvec_grep_\*` tool is present/,
  );
  assert.match(
    skill,
    /index lifecycle or daemon diagnostics, which are intentionally\s+kept out of the default agent MCP toolset/,
  );
  assert.match(
    skill,
    /default public MCP endpoint intentionally exposes only search and managed\s+ripgrep/,
  );
  assert.match(skill, /default Auto mode can select Server or Direct/);
  assert.match(
    skill,
    /do not probe forced Server mode and then retry forced Direct mode/,
  );
  assert.match(skill, /Call `zvec_grep_search` first/);
  assert.match(skill, /Call `zvec_grep_rg` for exhaustive local ripgrep/);
  assert.match(skill, /`freshness` and `indexing`/);
  assert.match(skill, /After authorization, use the CLI lifecycle workflow/);
  assert.doesNotMatch(skill, /zvec_grep_index(?:_drop|_status)?/);
  assert.doesNotMatch(skill, /zvec_grep_server_status/);
  assert.match(skill, /references\/cli-fallback\.md/);
  assert.doesNotMatch(skill, /Use zvec-grep through the `zg` command/);
  assert.match(metadata, /Repository search with explicit index lifecycle/);
  assert.match(
    metadata,
    /Prefer the public MCP search tools, and use the zg CLI only for authorized index lifecycle or daemon diagnostics/,
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
