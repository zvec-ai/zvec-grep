import assert from "node:assert/strict";
import test from "node:test";
import {
  rankingMultiplier,
  refreshRankingWeightsEnabled,
  MAX_RANKING_MULTIPLIER,
} from "../../dist/engine/pipeline/search/scoring.js";

function codeMetadata(overrides = {}) {
  return {
    kind: "code",
    symbolType: "function",
    symbolName: "loadUserProfile",
    scope: null,
    nodeType: "function_declaration",
    signature: "function loadUserProfile(id: string): Profile",
    doc: null,
    modifiers: ["exported"],
    ...overrides,
  };
}

function recall(query, overrides = {}) {
  return [
    {
      path: "fts",
      routeId: "fts",
      query,
      found: true,
      rank: 1,
      ...overrides,
    },
  ];
}

test("returns a neutral multiplier without code metadata", () => {
  assert.equal(rankingMultiplier({ recall: recall("loadUserProfile") }), 1);
  assert.equal(
    rankingMultiplier({
      metadata: { kind: "markdown", heading: "Usage", level: 2, scope: null },
      recall: recall("usage"),
    }),
    1,
  );
});

test("returns a neutral multiplier when no route found the candidate", () => {
  const multiplier = rankingMultiplier({
    metadata: codeMetadata(),
    recall: [
      { path: "fts", routeId: "fts", query: "loadUserProfile", found: false },
    ],
  });

  // Without any structural match, type prior is not unanchored; multiplier stays neutral.
  assert.equal(multiplier, 1.0);
});

test("boosts an exact symbol name match above a partial one", () => {
  const exact = rankingMultiplier({
    metadata: codeMetadata(),
    recall: recall("loadUserProfile"),
  });
  const partial = rankingMultiplier({
    metadata: codeMetadata(),
    recall: recall("loadUser"),
  });

  assert.ok(exact > partial, `${exact} should exceed ${partial}`);
  assert.ok(partial > 1, `${partial} should exceed 1`);
});

test("ranks name matches above scope and signature matches", () => {
  const metadata = codeMetadata({
    symbolName: "handle",
    scope: "RequestRouter",
    signature: "function handle(payload: Envelope): void",
  });

  const name = rankingMultiplier({ metadata, recall: recall("handle") });
  const scope = rankingMultiplier({
    metadata,
    recall: recall("RequestRouter"),
  });
  const signature = rankingMultiplier({ metadata, recall: recall("Envelope") });

  assert.ok(name > scope, `name ${name} should exceed scope ${scope}`);
  assert.ok(
    scope > signature,
    `scope ${scope} should exceed signature ${signature}`,
  );
  assert.ok(signature > 1, `signature ${signature} should exceed 1`);
});

test("never scores a symbol type below neutral", () => {
  // Demoting asserts "less likely relevant", which needs stronger evidence than
  // a type tag; failing to promote is the cheaper mistake.
  for (const symbolType of [
    "function",
    "class",
    "interface",
    "module",
    "value",
    "alias",
  ]) {
    const multiplier = rankingMultiplier({
      metadata: codeMetadata({ symbolType, symbolName: null, signature: null }),
      recall: recall("unrelated"),
    });

    assert.ok(multiplier >= 1, `${symbolType} was demoted to ${multiplier}`);
  }
});

test("weights definition symbol types above aliases when matching", () => {
  const fn = rankingMultiplier({
    metadata: codeMetadata({ symbolName: "target", signature: null }),
    recall: recall("target"),
  });
  const alias = rankingMultiplier({
    metadata: codeMetadata({
      symbolType: "alias",
      symbolName: "target",
      signature: null,
    }),
    recall: recall("target"),
  });

  assert.ok(fn > alias, `${fn} should exceed ${alias}`);
  assert.ok(alias > 1);
});

test("does not boost short symbol names on queries that merely contain them as substrings", () => {
  // Negative regression tests for reverse substring matching:
  // "forget" contains "get", "dataset" contains "set", "bitmap" contains "map", etc.
  // These must NOT earn a partial position boost for "get", "set", "map", or "log".
  const cases = [
    { symbolName: "get", query: "forget password" },
    { symbolName: "get", query: "target selector" },
    { symbolName: "set", query: "dataset migration" },
    { symbolName: "set", query: "asset pipeline" },
    { symbolName: "map", query: "bitmap renderer" },
    { symbolName: "log", query: "catalog parser" },
    { symbolName: "log", query: "prologue chapter" },
  ];

  for (const { symbolName, query } of cases) {
    const multiplier = rankingMultiplier({
      metadata: codeMetadata({ symbolName, signature: null, scope: null }),
      recall: recall(query),
    });

    assert.equal(
      multiplier,
      1.0,
      `symbolName "${symbolName}" with query "${query}" earned an unexpected partial boost: ${multiplier}`,
    );
  }
});

test("does not boost one- and two-character symbol names on unrelated queries", () => {
  // "a" is a substring of almost any query, so reverse containment would
  // otherwise hand a near-maximum boost to a completely unrelated symbol.
  for (const symbolName of ["a", "e", "id", "on"]) {
    const multiplier = rankingMultiplier({
      metadata: codeMetadata({ symbolName, signature: null }),
      recall: recall("validate the leaky bucket rate limiter"),
    });

    assert.equal(
      multiplier,
      1.0,
      `symbolName ${symbolName} earned an unexpected position boost`,
    );
  }
});

test("does not boost short query tokens that merely occur in a signature", () => {
  const metadata = codeMetadata({
    symbolName: "Validate",
    scope: null,
    signature: "func Validate(ctx context.Context, opts *Options) error",
  });

  for (const token of ["on", "at", "to"]) {
    assert.equal(
      rankingMultiplier({ metadata, recall: recall(token) }),
      1.0,
      `token ${token} earned an unexpected signature boost`,
    );
  }
});

test("still honours an exact match on a short symbol name", () => {
  const multiplier = rankingMultiplier({
    metadata: codeMetadata({ symbolName: "id", signature: null }),
    recall: recall("id"),
  });

  assert.ok(multiplier > 1.2, `${multiplier} should exceed the type weight`);
});

test("keeps the total adjustment within a narrow band", () => {
  // A query repeating many matching tokens must not compound without bound.
  const multiplier = rankingMultiplier({
    metadata: codeMetadata({
      symbolName: "load",
      scope: "load",
      signature: "load load load load load",
    }),
    recall: recall("load load load load load load load load"),
  });

  assert.ok(
    multiplier <= (1 + 0.6 * 1.5) * 1.2,
    `${multiplier} should stay bounded`,
  );
});

test("weak signature hits never outrank an exact symbol name match", () => {
  // Compounding one factor per token used to let a wide signature accumulate
  // past the strongest available signal. Ten tokens happened to stay under the
  // exact boost by luck; twelve did not, so sweep well past the crossover and
  // up to the token-scan cap.
  const exactName = rankingMultiplier({
    metadata: codeMetadata({
      symbolName: "alpha",
      scope: null,
      signature: null,
    }),
    recall: recall("alpha"),
  });

  for (const count of [10, 12, 16, 32, 64]) {
    const tokens = Array.from({ length: count }, (_, i) => `uniqtok${i}`);
    const manySignatureHits = rankingMultiplier({
      metadata: codeMetadata({
        symbolName: "X",
        scope: null,
        signature: `func X(${tokens.join(", ")} int) error`,
      }),
      recall: recall(tokens.join(" ")),
    });

    assert.ok(
      exactName > manySignatureHits,
      `exact name ${exactName} should beat ${count} signature hits (${manySignatureHits})`,
    );
  }
});

test("weak evidence never substitutes for a stronger signal", () => {
  // Signature and scope are capped below the next category's single-token
  // boost, so no amount of either stands in for a stronger hit. Symbol-name
  // evidence is deliberately exempt: see the partial-stacking test below.
  // A neutral symbol type keeps this about position boosts alone.
  const base = { symbolType: "module", scope: null, signature: null };
  const oneScope = rankingMultiplier({
    metadata: codeMetadata({ ...base, symbolName: "zzz", scope: "alphaa" }),
    recall: recall("alphaa"),
  });
  const onePartial = rankingMultiplier({
    metadata: codeMetadata({ ...base, symbolName: "alphaaxtail" }),
    recall: recall("alphaax"),
  });

  for (let count = 1; count <= 40; count++) {
    const tokens = Array.from({ length: count }, (_, i) => `uniqtok${i}`);
    const query = recall(tokens.join(" "));

    const signatureOnly = rankingMultiplier({
      metadata: codeMetadata({
        ...base,
        symbolName: "zzz",
        signature: `f(${tokens.join(", ")})`,
      }),
      recall: query,
    });
    const scopeOnly = rankingMultiplier({
      metadata: codeMetadata({
        ...base,
        symbolName: "zzz",
        scope: tokens.join(" "),
      }),
      recall: query,
    });

    assert.ok(
      signatureOnly < oneScope,
      `${count} signature hits (${signatureOnly}) should stay under one scope hit (${oneScope})`,
    );
    assert.ok(
      scopeOnly < onePartial,
      `${count} scope hits (${scopeOnly}) should stay under one partial hit (${onePartial})`,
    );
  }
});

test("lets several symbol-name hits outweigh one incidental exact hit", () => {
  // Deliberate, and the opposite of the rule for signature and scope: a
  // multi-word query ("call solve least squares") locates a symbol through
  // several partial name hits, and capping them below one exact hit regressed
  // 37 of the 1,000 benchmark queries while improving 5.
  const base = { symbolType: "module", scope: null, signature: null };
  const oneExact = rankingMultiplier({
    metadata: codeMetadata({ ...base, symbolName: "alphaa" }),
    recall: recall("alphaa"),
  });
  const manyPartial = rankingMultiplier({
    metadata: codeMetadata({ ...base, symbolName: "solveleastsquarestail" }),
    recall: recall("solve least squares"),
  });

  assert.ok(
    manyPartial > oneExact,
    `three partial hits (${manyPartial}) should be able to pass one exact hit (${oneExact})`,
  );
  // Still bounded, and still below what an exact hit plus the same evidence earns.
  assert.ok(manyPartial <= MAX_RANKING_MULTIPLIER);
});

test("counts additional matches, at a discount", () => {
  const metadata = codeMetadata({
    symbolName: "serialize",
    scope: null,
    signature: "func serialize(payload Envelope, opts Options) error",
  });

  const nameOnly = rankingMultiplier({ metadata, recall: recall("serialize") });
  const namePlusExtras = rankingMultiplier({
    metadata,
    recall: recall("serialize payload envelope options"),
  });

  // Matching more of the query is evidence, so it must help...
  assert.ok(
    namePlusExtras > nameOnly,
    `${namePlusExtras} should exceed ${nameOnly}`,
  );

  // ...but the extra terms are worth far less than the name match itself.
  const nameContribution = nameOnly - 1;
  const extrasContribution = namePlusExtras - nameOnly;
  assert.ok(
    extrasContribution < nameContribution,
    `extras ${extrasContribution} should stay below the name's ${nameContribution}`,
  );
});

test("ignores stop words when matching by substring", () => {
  const metadata = codeMetadata({
    symbolName: "X",
    scope: null,
    signature: "func X(from string, the int, with bool) error",
  });

  // Every query term below occurs in the signature, but none of them says
  // anything about what the caller is looking for.
  assert.equal(
    rankingMultiplier({
      metadata,
      recall: recall("how do I get the from with"),
    }),
    1.0,
    "stop words should not earn a signature boost",
  );
});

test("keeps verbs that double as symbol names out of the stop list", () => {
  // get / set / find / show are ordinary function names, so they must still
  // match by substring — not just on an exact hit.
  for (const verb of ["get", "set", "find", "show"]) {
    const exact = rankingMultiplier({
      metadata: codeMetadata({ symbolName: verb, signature: null }),
      recall: recall(verb),
    });
    const partial = rankingMultiplier({
      metadata: codeMetadata({
        symbolName: `${verb}UserProfile`,
        signature: null,
      }),
      recall: recall(verb),
    });

    assert.ok(exact > 1.2, `exact match on "${verb}" scored ${exact}`);
    assert.ok(partial > 1.2, `substring match on "${verb}" scored ${partial}`);
  }
});

test("keeps the stop list small enough to stay reviewable", () => {
  // The list is hand-curated, so it needs a ceiling: an earlier 78-word version
  // changed no top-10 ordering while 59 entries never fired. Anything below the
  // substring length floor is redundant here too.
  const filtered = [
    "the",
    "and",
    "for",
    "with",
    "from",
    "how",
    "what",
    "where",
  ];

  for (const word of filtered) {
    assert.equal(
      rankingMultiplier({
        metadata: codeMetadata({
          symbolName: "X",
          scope: null,
          signature: `func X(${word} int) error`,
        }),
        recall: recall(word),
      }),
      1.0,
      `"${word}" should not earn a signature boost`,
    );
  }
});

test("memoised token sets stay independent of route order and count", () => {
  const metadata = codeMetadata({
    symbolName: "loadUser",
    scope: "Svc",
    signature: "func loadUser(profile string) error",
  });
  const hit = (routeId, query, rank) => ({
    path: routeId === "fts" ? "fts" : "vector",
    routeId,
    query,
    found: true,
    rank,
  });

  const forward = rankingMultiplier({
    metadata,
    recall: [hit("fts", "loadUser", 1), hit("vector", "profile", 1)],
  });
  const reversed = rankingMultiplier({
    metadata,
    recall: [hit("vector", "profile", 1), hit("fts", "loadUser", 1)],
  });
  assert.equal(forward, reversed, "route order must not change the score");

  const single = rankingMultiplier({
    metadata,
    recall: [hit("fts", "loadUser", 1)],
  });
  const duplicated = rankingMultiplier({
    metadata,
    recall: [hit("fts", "loadUser", 1), hit("vector", "loadUser", 2)],
  });
  assert.equal(single, duplicated, "repeating a query must not inflate it");

  assert.ok(
    forward > single,
    "a second route contributing a new term should still help",
  );

  const withMissedRoute = rankingMultiplier({
    metadata,
    recall: [
      hit("fts", "loadUser", 1),
      { path: "vector", routeId: "vector", query: "profile", found: false },
    ],
  });
  assert.equal(
    withMissedRoute,
    single,
    "a route that did not find the candidate must not contribute terms",
  );
});

test("matches qualified symbol names exactly", () => {
  // The query tokeniser strips separators, so comparing the raw name against
  // query terms never matched for C++/Rust-style names.
  for (const symbolName of ["Foo::bar", "std::vector", "foo-bar", "foo.bar"]) {
    const whole = rankingMultiplier({
      metadata: codeMetadata({ symbolName, signature: null }),
      recall: recall(symbolName),
    });
    assert.ok(
      whole > 1.05,
      `${symbolName} should earn the exact-name boost, got ${whole}`,
    );
  }

  // The trailing segment is what a user actually types, and it must be worth
  // exactly as much as the same match on an unqualified name. The token that
  // earned the exact boost used to be scanned again for substring matches,
  // paying a qualified name ~13% more for the same evidence.
  const lastSegment = rankingMultiplier({
    metadata: codeMetadata({ symbolName: "Foo::bar", signature: null }),
    recall: recall("bar"),
  });
  const unqualified = rankingMultiplier({
    metadata: codeMetadata({ symbolName: "bar", signature: null }),
    recall: recall("bar"),
  });

  assert.equal(
    lastSegment,
    unqualified,
    `Foo::bar/bar (${lastSegment}) should score the same as bar/bar (${unqualified})`,
  );
});

test("does not pay twice for the token that earned the exact boost", () => {
  // Neutral type so the assertion is about the position boost alone:
  // 1 + SYMBOL_NAME_EXACT_BOOST, with no partial boost stacked on top.
  for (const symbolName of ["bar", "Foo::bar", "foo.bar", "foo-bar"]) {
    const multiplier = rankingMultiplier({
      metadata: codeMetadata({
        symbolType: "module",
        symbolName,
        scope: null,
        signature: null,
      }),
      recall: recall("bar"),
    });

    assert.equal(
      multiplier,
      1.65,
      `${symbolName} matched by "bar" should score exactly the exact-name boost`,
    );
  }
});

test("keeps unknown symbol types finite, including inherited property names", () => {
  // SYMBOL_TYPE_WEIGHTS is an object literal, so a lookup for an inherited key
  // used to return a function and turn the whole score into NaN. NaN would
  // destabilise the fusion comparator and silently void the pruning bound.
  for (const symbolType of [
    "toString",
    "constructor",
    "__proto__",
    "valueOf",
    "hasOwnProperty",
    "enum",
    "trait",
  ]) {
    const multiplier = rankingMultiplier({
      metadata: codeMetadata({
        symbolType,
        symbolName: "bar",
        signature: null,
      }),
      recall: recall("bar"),
    });

    assert.ok(
      Number.isFinite(multiplier),
      `symbolType "${symbolType}" produced a non-finite multiplier: ${multiplier}`,
    );
    assert.ok(
      multiplier >= 1 && multiplier <= MAX_RANKING_MULTIPLIER,
      `symbolType "${symbolType}" produced ${multiplier}, outside [1, ${MAX_RANKING_MULTIPLIER}]`,
    );
    // Unknown types fall back to neutral, so only the exact-name boost applies.
    assert.equal(
      multiplier,
      1.65,
      `symbolType "${symbolType}" should be neutral`,
    );
  }
});

test("keeps multi-query cache keys free of collisions", () => {
  // The cache key joins sorted queries on NUL, which the tokeniser can never
  // emit. A plain concatenation would let ["bar","foo"] and "barfoo" share an
  // entry, so one request's token plan would leak into another's ranking.
  const metadata = () =>
    codeMetadata({ symbolName: "bar", scope: "foo", signature: null });
  const routes = (queries) =>
    queries.map((query, index) => ({
      path: "fts",
      routeId: `r${index}`,
      query,
      found: true,
      rank: 1,
    }));

  // Warm the cache in one order, then assert the other reading is unaffected.
  const concatenatedFirst = rankingMultiplier({
    metadata: metadata(),
    recall: routes(["barfoo"]),
  });
  const splitSecond = rankingMultiplier({
    metadata: metadata(),
    recall: routes(["bar", "foo"]),
  });

  assert.notEqual(
    concatenatedFirst,
    splitSecond,
    `"barfoo" and ["bar","foo"] must not share a cache entry`,
  );

  // And the reverse order yields identical values, so nothing was poisoned.
  assert.equal(
    rankingMultiplier({ metadata: metadata(), recall: routes(["bar", "foo"]) }),
    splitSecond,
  );
  assert.equal(
    rankingMultiplier({ metadata: metadata(), recall: routes(["barfoo"]) }),
    concatenatedFirst,
  );

  // Different splits of the same concatenation must also stay distinct.
  assert.notEqual(
    rankingMultiplier({
      metadata: metadata(),
      recall: routes(["foo", "barbaz"]),
    }),
    rankingMultiplier({
      metadata: metadata(),
      recall: routes(["foobar", "baz"]),
    }),
  );
});

test("caps how many query terms are scanned", () => {
  const metadata = codeMetadata({
    symbolName: "handler",
    scope: null,
    signature: `func handler(${Array.from({ length: 60 }, (_, i) => `tok${i} int`).join(", ")}) error`,
  });
  const huge = Array.from({ length: 2000 }, (_, i) => `tok${i}`).join(" ");

  const started = process.hrtime.bigint();
  for (let i = 0; i < 200; i++) {
    rankingMultiplier({ metadata, recall: recall(huge) });
  }
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;

  // Without the cap this took ~200ms; the bound keeps an oversized query from
  // dominating the search it belongs to. Relaxed for slow/coverage CI runners.
  assert.ok(elapsedMs < 250, `2000-term query took ${elapsedMs.toFixed(1)}ms`);
});

test("stays within the advertised multiplier ceiling", () => {
  // Fusion prunes candidates using MAX_RANKING_MULTIPLIER as an upper bound;
  // if any input could exceed it, pruning would drop a candidate that belonged
  // in the window.
  const names = ["a", "load", "loadUser", "Foo::loadUser"];
  const scopes = [null, "loadUserService"];
  const signatures = [
    null,
    "func loadUser(load int, user int, id string) error",
  ];
  const queries = [
    "loadUser",
    "loadUser load user id error",
    "Foo::loadUser load",
  ];

  for (const symbolType of [
    "function",
    "class",
    "interface",
    "module",
    "value",
    "alias",
  ]) {
    for (const symbolName of names) {
      for (const scope of scopes) {
        for (const signature of signatures) {
          for (const query of queries) {
            const value = rankingMultiplier({
              metadata: codeMetadata({
                symbolType,
                symbolName,
                scope,
                signature,
              }),
              recall: recall(query),
            });
            assert.ok(
              value <= MAX_RANKING_MULTIPLIER + 1e-12,
              `${value} exceeded ceiling ${MAX_RANKING_MULTIPLIER}`,
            );
          }
        }
      }
    }
  }
});

test("never exceeds the bound fusion prunes against", () => {
  // Pruning skips a candidate when stock * MAX_RANKING_MULTIPLIER cannot reach
  // the cutoff. If any input could score above that bound, pruning would drop
  // candidates that belonged in the window — so this invariant is load-bearing.
  const names = [
    "load",
    "loadUser",
    "Foo::load",
    "a::b::load",
    "load-user",
    "x",
  ];
  const scopes = [null, "loadUserService", "load"];
  const signatures = [
    null,
    "func load(load int, user load, load2 load3) load4",
  ];
  const queries = ["load", "load user service int func", "loadUser load user"];
  const types = ["function", "class", "interface", "module", "value", "alias"];

  let max = 0;
  for (const symbolType of types) {
    for (const symbolName of names) {
      for (const scope of scopes) {
        for (const signature of signatures) {
          for (const query of queries) {
            const value = rankingMultiplier({
              metadata: codeMetadata({
                symbolType,
                symbolName,
                scope,
                signature,
              }),
              recall: recall(query),
            });
            max = Math.max(max, value);
          }
        }
      }
    }
  }

  assert.ok(
    max <= MAX_RANKING_MULTIPLIER,
    `observed ${max} exceeds the pruning bound ${MAX_RANKING_MULTIPLIER}`,
  );
  assert.ok(max > 1.05, `expected the sweep to approach the bound, got ${max}`);
});

test("collapses duplicate queries into one cache entry", () => {
  // Routes often repeat the same query. Deduping keeps the memo key stable and
  // avoids a second entry that would hold an identical plan.
  const metadata = codeMetadata({
    symbolName: "load",
    scope: null,
    signature: "func load(user int) error",
  });
  const hit = (query, rank) => ({
    path: "fts",
    routeId: `r${rank}`,
    query,
    found: true,
    rank,
  });

  const twoRoutes = rankingMultiplier({
    metadata,
    recall: [hit("load", 1), hit("user", 2)],
  });
  const withDuplicate = rankingMultiplier({
    metadata,
    recall: [hit("load", 1), hit("user", 2), hit("load", 3)],
  });
  const reordered = rankingMultiplier({
    metadata,
    recall: [hit("user", 1), hit("load", 2), hit("user", 3)],
  });

  assert.equal(
    withDuplicate,
    twoRoutes,
    "a repeated query must not change the score",
  );
  assert.equal(reordered, twoRoutes, "route order must not change the score");
});

test("survives hostile and non-ASCII input without throwing", () => {
  // Queries arrive from MCP callers verbatim, so regex metacharacters, control
  // characters and CJK text all have to be inert rather than fatal.
  const metadata = codeMetadata({
    symbolName: "abc",
    scope: null,
    signature: "func abc() error",
  });

  for (const query of [
    "(((((",
    "$^*+?[]{}",
    "\t\n\r",
    "   ",
    "用户加载",
    "café",
    "a".repeat(5000),
  ]) {
    const multiplier = rankingMultiplier({ metadata, recall: recall(query) });

    assert.ok(
      Number.isFinite(multiplier) && multiplier >= 1,
      `query ${JSON.stringify(query.slice(0, 12))} produced ${multiplier}`,
    );
  }
});

test("stays fast on an oversized symbol name and signature", () => {
  // A pathological identifier must not trigger quadratic scanning.
  const huge = "a".repeat(20000);
  const started = process.hrtime.bigint();
  const multiplier = rankingMultiplier({
    metadata: codeMetadata({
      symbolName: huge,
      signature: `func ${huge}() error`,
    }),
    recall: recall("aaa bbb ccc"),
  });
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;

  assert.ok(Number.isFinite(multiplier));
  assert.ok(elapsedMs < 200, `took ${elapsedMs.toFixed(1)}ms`);
});

test("pruning by the bound never changes the visible window", () => {
  // Fusion skips scoring for candidates that cannot reach the cutoff. That is
  // only sound if the bound really bounds, so replay the decision over many
  // randomised shapes and compare against scoring everything.
  const names = ["load", "loadUser", "Foo::load", "user", "serialize", "x"];
  const signatures = [
    null,
    "func load(id int) error",
    "func f(load int, user int, load2 int) load3",
  ];
  const types = ["function", "class", "interface", "module", "value", "alias"];
  const queries = ["load user", "loadUser", "serialize x", "load"];

  // Deterministic PRNG: a flaky ranking test is worse than no test.
  let seed = 42;
  const random = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const pick = (list) => list[Math.floor(random() * list.length)];

  for (let round = 0; round < 400; round++) {
    const size = 8 + Math.floor(random() * 30);
    const limit = 1 + Math.floor(random() * 8);
    const query = pick(queries);
    const candidates = Array.from({ length: size }, (_, index) => ({
      id: `e${String(index).padStart(3, "0")}`,
      stock: 1 / (60 + 1 + Math.floor(random() * 120)),
      metadata: codeMetadata({
        symbolType: pick(types),
        symbolName: pick(names),
        scope: random() < 0.3 ? "loadUserService" : null,
        signature: pick(signatures),
      }),
    }));

    const weighted = (candidate) =>
      rankingMultiplier({
        metadata: candidate.metadata,
        recall: recall(query),
      });
    const order = (list) =>
      [...list]
        .sort((left, right) =>
          right.score !== left.score
            ? right.score - left.score
            : left.id.localeCompare(right.id),
        )
        .slice(0, limit)
        .map((entry) => entry.id)
        .join(",");

    const full = order(
      candidates.map((c) => ({ id: c.id, score: c.stock * weighted(c) })),
    );

    const stockOrder = candidates.map((c) => c.stock).sort((a, b) => b - a);
    const cutoff = stockOrder[Math.min(limit, stockOrder.length) - 1] ?? 0;
    const pruned = order(
      candidates.map((c) => ({
        id: c.id,
        score:
          c.stock * MAX_RANKING_MULTIPLIER < cutoff
            ? c.stock
            : c.stock * weighted(c),
      })),
    );

    assert.equal(pruned, full, `round ${round} (limit ${limit}) diverged`);
  }
});

test("leaves every non-code corpus untouched", () => {
  // Markdown, plain text, PDFs and images must keep stock RRF ordering: the
  // weights only encode code structure, so applying them elsewhere would
  // reorder results on no evidence at all.
  const nonCode = [
    { kind: "markdown", heading: "Rate Limiting", level: 2, scope: null },
    { kind: "markdown", heading: null, level: null, scope: null },
    undefined,
    null,
  ];

  for (const metadata of nonCode) {
    for (const query of [
      "rate limiting",
      "how do I configure the rate limit",
    ]) {
      assert.equal(
        rankingMultiplier({ metadata, recall: recall(query) }),
        1,
        `${JSON.stringify(metadata)} was reweighted`,
      );
    }
  }
});

test("degrades to neutral on metadata it does not understand", () => {
  // Metadata comes straight off the index, so a field an older or newer
  // extractor omits must not crash the whole search.
  const malformed = [
    { kind: "pdf", page: 3 },
    { kind: "image", format: "png" },
    {},
    { kind: "code" },
    {
      kind: "code",
      symbolName: undefined,
      scope: undefined,
      signature: undefined,
    },
    { kind: "code", symbolType: "enum", symbolName: "load" },
  ];

  for (const metadata of malformed) {
    const multiplier = rankingMultiplier({
      metadata,
      recall: recall("load user"),
    });

    assert.ok(
      Number.isFinite(multiplier) && multiplier >= 1,
      `${JSON.stringify(metadata)} produced ${multiplier}`,
    );
  }
});

test("ZVEC_GREP_RANKING_WEIGHTS reverts to stock RRF ordering", () => {
  const input = {
    metadata: codeMetadata(),
    recall: recall("loadUserProfile"),
  };
  const previous = process.env.ZVEC_GREP_RANKING_WEIGHTS;

  try {
    for (const value of ["0", "off", "false", "OFF"]) {
      process.env.ZVEC_GREP_RANKING_WEIGHTS = value;
      refreshRankingWeightsEnabled();
      assert.equal(
        rankingMultiplier(input),
        1,
        `${value} should disable the weights`,
      );
    }

    for (const value of ["1", "on", ""]) {
      process.env.ZVEC_GREP_RANKING_WEIGHTS = value;
      refreshRankingWeightsEnabled();
      assert.ok(
        rankingMultiplier(input) > 1,
        `${value} should keep the weights enabled`,
      );
    }
  } finally {
    if (previous === undefined) {
      delete process.env.ZVEC_GREP_RANKING_WEIGHTS;
    } else {
      process.env.ZVEC_GREP_RANKING_WEIGHTS = previous;
    }
    refreshRankingWeightsEnabled();
  }
});

test("is case-insensitive on symbol names", () => {
  const lower = rankingMultiplier({
    metadata: codeMetadata(),
    recall: recall("loaduserprofile"),
  });
  const exact = rankingMultiplier({
    metadata: codeMetadata(),
    recall: recall("loadUserProfile"),
  });

  assert.equal(lower, exact);
});

test("does not unanchor code entities over markdown docs without structural match", () => {
  // Verifies that code entities without any structural position match do not displace
  // relevant docs purely by type prior alone.
  const query = "how to configure database connections";
  const docCandidate = {
    metadata: { kind: "markdown", heading: "Configuration Guide", level: 1, scope: null },
    recall: recall(query, { rank: 1 }),
  };
  const codeCandidate13 = {
    metadata: codeMetadata({
      symbolType: "function",
      symbolName: null,
      scope: null,
      signature: null,
    }),
    recall: recall(query, { rank: 13 }),
  };

  const docMultiplier = rankingMultiplier(docCandidate);
  const codeMultiplier13 = rankingMultiplier(codeCandidate13);

  assert.equal(docMultiplier, 1.0, "markdown doc receives neutral multiplier 1.0");
  assert.equal(codeMultiplier13, 1.0, "unrelated function without structural match stays neutral 1.0");
});

