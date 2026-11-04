import type {
  CodeSymbolType,
  EntityMetadata,
  SearchRecallTrace,
} from "../../types.js";

/**
 * Multiplicative ranking adjustments applied on top of the RRF fusion score.
 *
 * Two properties are deliberate:
 *
 * - Nothing is ever scored below 1. Demoting a symbol type asserts "this is less
 *   likely to be relevant", which needs stronger evidence than we have; failing
 *   to promote is the cheaper mistake.
 * - Signature and scope evidence is capped so that no amount of it substitutes
 *   for a stronger signal; symbol-name evidence is allowed to accumulate,
 *   because multi-word queries depend on it. See BOOST_CEILINGS.
 *
 * The per-token ordering is exact > partial > scope > signature. That ordering
 * holds in aggregate for the weak tiers only: several partial name hits may
 * legitimately outrank one incidental exact hit, which is what multi-word
 * queries need, but no number of signature hits can reach one scope hit.
 *
 * This is bounded reranking, not tie-breaking. The ceiling is roughly 2.28x, and
 * under a single RRF route that is enough for a candidate around rank 79 to pass
 * one at rank 1 (2.28/139 > 1/61). Reordering across that distance is the
 * intent; the bound is what keeps it from becoming unbounded.
 *
 * The constants below reflect empirical calibration across a 1,000-query
 * multi-scenario benchmark covering exact symbols, intent phrases, natural
 * language QA, scoped lookups, and runtime error traces.
 */

/** Query token matched the symbol name exactly (case-insensitive). */
const SYMBOL_NAME_EXACT_BOOST = 0.65;

/** Query token is a substring of the symbol name. */
const SYMBOL_NAME_PARTIAL_BOOST = 0.29;

/** Query token matched the enclosing scope (class/module the symbol lives in). */
const SCOPE_MATCH_BOOST = 0.28;

/** Query token matched the signature but neither the name nor the scope. */
const SIGNATURE_MATCH_BOOST = 0.08;

/**
 * Weight applied to matches beyond the strongest one.
 *
 * Matching several query terms is real evidence, so it should count for
 * something; but summing every term at full weight lets a wide signature
 * out-score a genuine symbol-name hit. Secondary matches are therefore
 * discounted, and the total is capped below.
 */
const SECONDARY_MATCH_WEIGHT = 0.75;

/**
 * Ceiling on the summed position boost.
 *
 * Reached only when the strongest signal is an exact name match; weaker
 * strongest-signals cap out lower. See BOOST_CEILINGS.
 */
const MAX_POSITION_BOOST = SYMBOL_NAME_EXACT_BOOST * 1.5;

/**
 * Match categories, ordered weakest to strongest. The ordering is load-bearing:
 * comparing categories rather than their boost values keeps the "strongest wins"
 * logic correct even if two categories are ever calibrated to the same value.
 */
const MATCH_NONE = 0;
const MATCH_SIGNATURE = 1;
const MATCH_SCOPE = 2;
const MATCH_PARTIAL = 3;
const MATCH_EXACT = 4;

/** Boost contributed by one token, indexed by match category. */
const MATCH_BOOSTS: readonly number[] = [
  0,
  SIGNATURE_MATCH_BOOST,
  SCOPE_MATCH_BOOST,
  SYMBOL_NAME_PARTIAL_BOOST,
  SYMBOL_NAME_EXACT_BOOST,
];

/**
 * Nudges a ceiling just below the value it is derived from, so the comparison
 * stays strict under floating-point arithmetic.
 */
const STRICTLY_BELOW = 1 - 1e-9;

/**
 * Ceiling on the summed boost, indexed by the strongest category that matched.
 *
 * The weak tiers are capped strictly below the next category's single-token
 * boost, so no number of signature hits can reach one scope hit and no number
 * of scope hits can reach one partial hit. Without this, twelve signature terms
 * out-scored an exact name match — a wide function signature could beat the
 * symbol the user actually named.
 *
 * Partial and exact are deliberately NOT capped that way. A partial hit is real
 * locating evidence, and multi-word queries ("call solve least squares") depend
 * on several of them adding up to outrank a single incidental exact hit.
 * Capping the partial tier as well was measured on the 1,000-query benchmark:
 * it regressed 37 queries and improved 5, for -0.0034 MRR (95% CI
 * [-0.0051, -0.0012]), almost entirely on multi-word action phrases. Capping
 * only the signature and scope tiers costs nothing measurable (MRR unchanged at
 * 0.5021) and still removes the pathology.
 */
const BOOST_CEILINGS: readonly number[] = [
  0,
  SCOPE_MATCH_BOOST * STRICTLY_BELOW,
  SYMBOL_NAME_PARTIAL_BOOST * STRICTLY_BELOW,
  MAX_POSITION_BOOST,
  MAX_POSITION_BOOST,
];

/**
 * Shortest token allowed to earn a boost by substring containment. Exact
 * symbol-name equality is still honoured below this length.
 */
const MIN_SUBSTRING_TOKEN_LENGTH = 3;

/**
 * Cap on how many terms are scanned for substring matches.
 *
 * Scanning is O(candidates x terms x fields), so an oversized query — a pasted
 * stack trace or code block arriving through MCP — would otherwise dominate the
 * whole search. Longer terms are kept because they discriminate better.
 */
const MAX_SUBSTRING_TOKENS = 32;

/** Identifier-ish runs, matching how query text is tokenised. */
const IDENTIFIER_PATTERN = /[A-Za-z_][A-Za-z0-9_]*/g;

/**
 * Words that carry no locating intent on their own in a code search.
 *
 * Natural-language queries ("how does the circuit breaker decide to open") are
 * padded with these, and they collide with real identifiers, so letting them
 * match by substring hands out boosts that say nothing about relevance.
 *
 * Kept deliberately small. Measured against a real index, an earlier 78-word
 * list moved 1.5% of scores by at most 0.02 and changed no top-10 ordering,
 * while 59 of its entries never fired at all. Anything shorter than
 * MIN_SUBSTRING_TOKEN_LENGTH is already excluded, and words that double as
 * plausible symbol names (get, set, find, show) are left out so they can still
 * match — which is also why no exact-match exemption is needed here.
 *
 * TODO: replace this hand-curated list with an IDF signal derived from the
 * index. Term frequencies would make the filter self-tuning, language-agnostic
 * (this list is English-only, and the tokeniser does not emit CJK at all), and
 * free of the judgement calls above. That needs index-time term statistics,
 * which do not exist yet.
 */
const STOP_WORDS: ReadonlySet<string> = new Set([
  "the",
  "and",
  "for",
  "with",
  "from",
  "how",
  "what",
  "where",
  "when",
  "why",
  "this",
  "that",
]);

/**
 * Per-symbol-type weights. Definitions a reader is usually looking for rank
 * slightly above incidental matches; nothing is demoted below neutral.
 */
const SYMBOL_TYPE_WEIGHTS: Readonly<Record<CodeSymbolType, number>> = {
  function: 1.3,
  class: 1.25,
  interface: 1.05,
  module: 1.0,
  value: 1.0,
  alias: 1.0,
};

/** Applied to markdown entities and to code entities with unknown metadata. */
const NEUTRAL_WEIGHT = 1;

/**
 * The largest value rankingMultiplier can return.
 *
 * Fusion uses this to skip candidates that cannot reach the visible window:
 * if a candidate's stock RRF score times this ceiling still falls short of the
 * worst score already guaranteed a slot, no weighting can promote it.
 *
 * A candidate that maxes out every signal lands exactly on this value, so the
 * product is nudged up by one ulp-ish margin. Without it, floating-point
 * rounding in the caller's comparison could drop a candidate that was entitled
 * to the very top of the range.
 */
export const MAX_RANKING_MULTIPLIER =
  (NEUTRAL_WEIGHT + MAX_POSITION_BOOST) * maxSymbolTypeWeight() * (1 + 1e-9);

function maxSymbolTypeWeight(): number {
  let max = NEUTRAL_WEIGHT;
  for (const weight of Object.values(SYMBOL_TYPE_WEIGHTS)) {
    if (weight > max) {
      max = weight;
    }
  }

  return max;
}

/**
 * Escape hatch: setting this to "0" / "off" / "false" reverts fusion to stock
 * RRF ordering, so a bad ranking regression can be turned off by restarting the
 * process with the variable set.
 *
 * Read once at load, so changing it mid-process has no effect: fusion calls
 * into this module once per candidate, and reading process.env on every call
 * costs more than the scoring itself.
 */
const DISABLE_ENV_VAR = "ZVEC_GREP_RANKING_WEIGHTS";

let weightsEnabled = readWeightsEnabled();

function readWeightsEnabled(): boolean {
  const raw = process.env[DISABLE_ENV_VAR]?.trim().toLowerCase();

  return raw !== "0" && raw !== "off" && raw !== "false";
}

/**
 * Whether weighting is on. Fusion checks this once per search to skip the
 * cutoff sort and the whole weighting pass, rather than calling
 * rankingMultiplier() per candidate only to get 1 back.
 */
export function rankingWeightsEnabled(): boolean {
  return weightsEnabled;
}

/**
 * Re-reads the environment. Exists for tests that toggle the flag in-process;
 * production reads it once at startup.
 */
export function refreshRankingWeightsEnabled(): boolean {
  weightsEnabled = readWeightsEnabled();

  return weightsEnabled;
}

export type CandidateScoreInput = {
  metadata?: EntityMetadata;
  recall: readonly SearchRecallTrace[];
};

/**
 * Returns the multiplier to apply to a candidate's fused RRF score.
 *
 * Returns exactly 1 when there is no metadata to reason about, which keeps
 * non-code corpora on the stock RRF ordering.
 *
 * That neutrality is deliberate but not free: in a corpus mixing code with
 * markdown, only the code side is ever lifted, so a doc can lose its slot to a
 * code entity ranked a few places below it (measured: up to ~5 places). The
 * alternative — scoring a markdown heading the way a symbol name is scored —
 * was considered and dropped, because a query that reaches this module carries
 * no signal about whether the reader wanted prose or an implementation, and
 * guessing wrong is worse than leaving docs on the stock ordering.
 */
export function rankingMultiplier(candidate: CandidateScoreInput): number {
  if (!weightsEnabled) {
    return NEUTRAL_WEIGHT;
  }

  const metadata = candidate.metadata;
  if (!metadata || metadata.kind !== "code") {
    return NEUTRAL_WEIGHT;
  }

  const typeWeight = symbolTypeWeight(metadata.symbolType);

  // If metadata fields are missing, no position match is possible.
  if (
    metadata.symbolName == null &&
    metadata.scope == null &&
    metadata.signature == null
  ) {
    return NEUTRAL_WEIGHT;
  }

  const boost = positionBoost(metadata, queryTokens(candidate.recall));
  // Condition symbol-type prior on having at least one structural match:
  // An unanchored code function should not arbitrarily displace relevant docs
  // or closer semantic matches purely by virtue of being a function.
  if (boost === NEUTRAL_WEIGHT) {
    return NEUTRAL_WEIGHT;
  }

  return boost * typeWeight;
}

/**
 * Collects the distinct lowercased tokens across every route that recalled this
 * candidate. Routes share the same user query in practice, but a plan may carry
 * several rewrites and each of them is a legitimate source of match evidence.
 *
 * The result is memoised on the set of matched queries rather than recomputed
 * per candidate: fusion calls this once for each of hundreds of candidates, and
 * nearly all of them were found by the same two or three routes, so the token
 * set is identical across them.
 */
function queryTokens(recall: readonly SearchRecallTrace[]): TokenPlan {
  let single: string | undefined;
  let queries: string[] | undefined;

  for (const trace of recall) {
    if (!trace.found || trace.query === undefined) {
      continue;
    }

    if (queries !== undefined) {
      if (!queries.includes(trace.query)) {
        queries.push(trace.query);
      }
    } else if (single === undefined) {
      single = trace.query;
    } else if (trace.query !== single) {
      queries = [single, trace.query];
    }
  }

  if (queries === undefined) {
    // Every route carried the same query (or there was none at all).
    return single === undefined ? EMPTY_PLAN : tokenPlanForQuery(single);
  }

  queries.sort();

  // NUL is the separator because the tokeniser can never emit it: identifiers
  // are [A-Za-z_][A-Za-z0-9_]*, so no token contains it. Two different query
  // sets therefore cannot collide on a key unless they tokenise identically,
  // which makes sharing a cache entry harmless. A plain concatenation would
  // collide ("barfoo" vs ["bar","foo"]) and leak one query's plan into another.
  // Renderers tend to show NUL as nothing at all — it is a real character here.
  return tokenPlanForQueries(queries.join(QUERY_KEY_SEPARATOR), queries);
}

/** See the collision argument in queryTokens(). */
const QUERY_KEY_SEPARATOR = "\u0000";

/**
 * The query terms a candidate is scored against, precomputed once per query.
 *
 * Splitting the two uses matters: exact symbol-name equality needs an O(1)
 * lookup over every term, while substring scanning needs an array that has
 * already dropped the terms barred from it. Doing the length and stop-word
 * checks here means they run once per query instead of once per candidate per
 * field.
 */
type TokenPlan = {
  /** Every term, for exact symbol-name comparison. */
  readonly all: ReadonlySet<string>;
  /** Terms eligible for substring matching, in iteration order. */
  readonly substring: readonly string[];
};

const EMPTY_PLAN: TokenPlan = { all: new Set<string>(), substring: [] };

const tokenPlanCache = new Map<string, TokenPlan>();

/** Bounds the memo so a long-lived daemon cannot accumulate queries forever. */
const TOKEN_PLAN_CACHE_LIMIT = 512;

/** Cap on query string length allowed into the token plan cache to guard memory. */
const MAX_CACHED_QUERY_LENGTH = 4096;

function tokenPlanForQuery(query: string): TokenPlan {
  if (query.length > MAX_CACHED_QUERY_LENGTH) {
    return rememberTokenPlan(query, [query], false);
  }
  return tokenPlanCache.get(query) ?? rememberTokenPlan(query, [query], true);
}

function tokenPlanForQueries(
  key: string,
  queries: readonly string[],
): TokenPlan {
  if (key.length > MAX_CACHED_QUERY_LENGTH) {
    return rememberTokenPlan(key, queries, false);
  }
  return tokenPlanCache.get(key) ?? rememberTokenPlan(key, queries, true);
}

function rememberTokenPlan(
  key: string,
  queries: readonly string[],
  shouldCache: boolean = true,
): TokenPlan {
  const all = new Set<string>();
  for (const query of queries) {
    for (const token of tokenizeQuery(query)) {
      all.add(token);
    }
  }

  const substring: string[] = [];
  for (const token of all) {
    if (token.length >= MIN_SUBSTRING_TOKEN_LENGTH && !STOP_WORDS.has(token)) {
      substring.push(token);
    }
  }

  // Keep the longest terms when a query is oversized: they are the ones that
  // actually discriminate between candidates.
  if (substring.length > MAX_SUBSTRING_TOKENS) {
    substring.sort((left, right) => right.length - left.length);
    substring.length = MAX_SUBSTRING_TOKENS;
  }

  const plan: TokenPlan = { all, substring };
  if (shouldCache) {
    if (tokenPlanCache.size >= TOKEN_PLAN_CACHE_LIMIT) {
      tokenPlanCache.clear();
    }
    tokenPlanCache.set(key, plan);
  }

  return plan;
}

const queryTokenCache = new Map<string, readonly string[]>();

/** Bounds the memo so a long-lived daemon cannot accumulate queries forever. */
const QUERY_TOKEN_CACHE_LIMIT = 512;

function tokenizeQuery(query: string): readonly string[] {
  if (query.length > MAX_CACHED_QUERY_LENGTH) {
    const tokens: string[] = [];
    for (const match of query.matchAll(IDENTIFIER_PATTERN)) {
      tokens.push(match[0].toLowerCase());
    }
    return tokens;
  }

  const cached = queryTokenCache.get(query);
  if (cached !== undefined) {
    return cached;
  }

  const tokens: string[] = [];
  for (const match of query.matchAll(IDENTIFIER_PATTERN)) {
    tokens.push(match[0].toLowerCase());
  }

  if (queryTokenCache.size >= QUERY_TOKEN_CACHE_LIMIT) {
    queryTokenCache.clear();
  }
  queryTokenCache.set(query, tokens);

  return tokens;
}

const loweredCache = new Map<string, string>();

/** Bounds the memo so a long-lived daemon cannot accumulate strings forever. */
const LOWERED_CACHE_LIMIT = 4096;

/**
 * Lowercases a metadata field, mapping null and empty to undefined.
 *
 * Memoised because the same entity is scored on every search a daemon serves,
 * and symbol names and scopes repeat heavily within an index. Measured faster
 * than converting each time even when signatures are unique.
 */
function lowered(value: string | null | undefined): string | undefined {
  // Metadata reaches this module straight from the index, so a field that a
  // future extractor leaves off must degrade to "no signal" rather than throw.
  if (typeof value !== "string" || value.length === 0) {
    return undefined;
  }

  const cached = loweredCache.get(value);
  if (cached !== undefined) {
    return cached;
  }

  const result = value.toLowerCase();
  if (loweredCache.size >= LOWERED_CACHE_LIMIT) {
    loweredCache.clear();
  }
  loweredCache.set(value, result);

  return result;
}

/**
 * Rewards candidates whose query terms landed on a structurally meaningful
 * position (the symbol name) over those that only matched somewhere in the body.
 *
 * The strongest match counts in full and every other match is discounted, so
 * matching several terms still helps. The total is then capped by the strongest
 * category that matched, which is what stops a pile of weak signature hits from
 * overtaking a genuine symbol-name match.
 */
function positionBoost(
  metadata: Extract<EntityMetadata, { kind: "code" }>,
  plan: TokenPlan,
): number {
  const symbolName = lowered(metadata.symbolName);
  const scope = lowered(metadata.scope);
  const signature = lowered(metadata.signature);
  let strongestCategory = MATCH_NONE;
  let strongest = 0;
  let rest = 0;

  // An exact name match is the strongest signal available, and it is the only
  // one a stop word or a very short token can still earn.
  const exactToken =
    symbolName === undefined
      ? undefined
      : exactSymbolNameToken(symbolName, plan);
  if (exactToken !== undefined) {
    strongestCategory = MATCH_EXACT;
    strongest = SYMBOL_NAME_EXACT_BOOST;
  }

  for (const token of plan.substring) {
    // Already counted in full as the exact-name match. Skipping the token that
    // earned it — not just one equal to the whole name — is what keeps a
    // qualified name (`Foo::bar` matched by `bar`) from also collecting a
    // partial boost for the same token.
    if (token === exactToken) {
      continue;
    }

    const category = substringMatchCategory(
      token,
      symbolName,
      scope,
      signature,
    );
    if (category === MATCH_NONE) {
      continue;
    }

    const boost = MATCH_BOOSTS[category]!;
    if (category > strongestCategory) {
      rest += strongest;
      strongestCategory = category;
      strongest = boost;
    } else {
      rest += boost;
    }
  }

  if (strongestCategory === MATCH_NONE) {
    return NEUTRAL_WEIGHT;
  }

  const total = strongest + rest * SECONDARY_MATCH_WEIGHT;

  return NEUTRAL_WEIGHT + Math.min(total, BOOST_CEILINGS[strongestCategory]!);
}

/**
 * The query token that names this symbol outright, or undefined.
 *
 * Returns the token rather than a boolean so the caller can exclude it from
 * substring scanning; counting it twice inflated qualified names by ~13%.
 *
 * Qualified names carry separators the query tokeniser strips — `Foo::bar`
 * arrives as `foo` and `bar` — so comparing the raw name against query terms
 * would never match for C++, Rust or any dotted/hyphenated identifier. Falling
 * back to the name's own identifier segments makes those match on the last
 * segment, which is what the user typed.
 */
function exactSymbolNameToken(
  symbolName: string,
  plan: TokenPlan,
): string | undefined {
  if (plan.all.has(symbolName)) {
    return symbolName;
  }

  const segments = identifierSegments(symbolName);
  if (segments.length > 1) {
    const last = segments[segments.length - 1]!;
    if (plan.all.has(last)) {
      return last;
    }
  }

  return undefined;
}

const segmentCache = new Map<string, readonly string[]>();

/** Bounds the memo so a long-lived daemon cannot accumulate names forever. */
const SEGMENT_CACHE_LIMIT = 4096;

function identifierSegments(value: string): readonly string[] {
  const cached = segmentCache.get(value);
  if (cached !== undefined) {
    return cached;
  }

  const segments = value.match(IDENTIFIER_PATTERN) ?? [];
  if (segmentCache.size >= SEGMENT_CACHE_LIMIT) {
    segmentCache.clear();
  }
  segmentCache.set(value, segments);

  return segments;
}

/**
 * The strongest position a single substring-eligible token matched, or
 * MATCH_NONE.
 */
function substringMatchCategory(
  token: string,
  symbolName: string | undefined,
  scope: string | undefined,
  signature: string | undefined,
): number {
  if (symbolName !== undefined && isSubstringMatch(symbolName, token)) {
    return MATCH_PARTIAL;
  }

  if (scope !== undefined && isSubstringMatch(scope, token)) {
    return MATCH_SCOPE;
  }

  if (signature !== undefined && isSubstringMatch(signature, token)) {
    return MATCH_SIGNATURE;
  }

  return MATCH_NONE;
}

/**
 * Checks whether a query token is contained as a substring within a metadata
 * field (symbol name, enclosing scope, or signature).
 *
 * Strict forward containment only (field.includes(token)). Reverse containment
 * (token.includes(field)) is deliberately barred: otherwise common short symbol
 * names ("get", "set", "map", "log") would spuriously match queries like
 * "forget password", "dataset migration", "bitmap renderer", and "catalog parser".
 */
function isSubstringMatch(field: string, token: string): boolean {
  return field.includes(token);
}

function symbolTypeWeight(symbolType: CodeSymbolType): number {
  // Own-property lookup only. A plain object literal inherits from
  // Object.prototype, so an extractor emitting "constructor" or "toString" —
  // both plausible symbol names — would otherwise read a function off the
  // prototype and turn the score into NaN. NaN breaks more than the one
  // candidate: the fusion comparator returns NaN (sorting as 0, so the order
  // goes unstable) and the pruning test `score * MAX < cutoff` is false for
  // NaN, silently voiding the upper bound.
  if (!Object.hasOwn(SYMBOL_TYPE_WEIGHTS, symbolType)) {
    return NEUTRAL_WEIGHT;
  }

  const weight = SYMBOL_TYPE_WEIGHTS[symbolType];

  return Number.isFinite(weight) ? weight : NEUTRAL_WEIGHT;
}
