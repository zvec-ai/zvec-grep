import assert from "node:assert/strict";
import test from "node:test";
import { fullTextQuery } from "../../dist/engine/storage/fts-query.js";

test("lexical queries ignore separators without exposing a query language", () => {
  assert.equal(
    fullTextQuery("network\tbackoff\npolicy?"),
    '"network" OR "backoff" OR "policy"',
  );
  assert.equal(
    fullTextQuery('text:foo AND NOT (bar*) \\"'),
    '"text" OR "foo" OR "AND" OR "NOT" OR "bar"',
  );
  assert.equal(fullTextQuery("$foo_bar.foo"), '"foo" OR "bar"');
  assert.equal(fullTextQuery("123"), '"123"');
  for (const query of ["", " ", "\t\n", "_", "()", "😀", "\u0301"]) {
    assert.equal(fullTextQuery(query), undefined);
  }
});

test("Unicode terms keep native segmentation and combining marks", () => {
  assert.equal(fullTextQuery("网络释放模型"), "网络释放模型");
  assert.equal(
    fullTextQuery("查询 ModelPool 的状态？"),
    '查询 OR "ModelPool" OR 的状态',
  );
  assert.equal(fullTextQuery("café cafe\u0301"), "café OR cafe\u0301");
  assert.equal(fullTextQuery("システム שלום"), "システム OR שלום");
});
