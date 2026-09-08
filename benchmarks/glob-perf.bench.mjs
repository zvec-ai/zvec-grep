import { readdirSync } from "node:fs";
import { join, relative } from "node:path";
import { performance } from "node:perf_hooks";
import assert from "node:assert/strict";
import {
  pathPatternMatches as optimized_pathPatternMatches,
  ripgrepGlobMatches as optimized_ripgrepGlobMatches,
  pathPatternMightMatchDescendant as optimized_pathPatternMightMatchDescendant,
  normalizePathForMatch,
  normalizePathPattern,
  isAbsolutePathPattern,
  hasPathGlob,
} from "../dist/engine/utils/glob.js";

// ============================================================================
// 1. Unoptimized Baseline Implementation (for 1:1 comparison & validation)
// ============================================================================

function baseline_globFragmentToRegExp(pattern) {
  let expression = "";
  for (let index = 0; index < pattern.length; index++) {
    const char = pattern[index];
    const next = pattern[index + 1];
    const afterNext = pattern[index + 2];

    if (char === "*" && next === "*" && afterNext === "/") {
      expression += "(?:.*/)?";
      index += 2;
    } else if (char === "*" && next === "*") {
      expression += ".*";
      index++;
    } else if (char === "*") {
      expression += "[^/]*";
    } else if (char === "?") {
      expression += "[^/]";
    } else if (char === "[") {
      const endIndex = pattern.indexOf("]", index + 1);
      if (endIndex >= 0) {
        let content = pattern.slice(index + 1, endIndex);
        if (content && content !== "!" && content !== "^") {
          const negated = content.startsWith("!") || content.startsWith("^");
          if (negated) {
            content = content.slice(1);
          }
          content = content.replaceAll("\\", "\\\\").replaceAll("/", "\\/");
          expression += `[${negated ? "^" : ""}${content}]`;
          index = endIndex;
          continue;
        }
      }
      expression += "\\[";
    } else if (char === "{") {
      let depth = 0;
      let alternativeStart = index + 1;
      let matched = false;
      const alternatives = [];
      for (let i = index + 1; i < pattern.length; i++) {
        const c = pattern[i];
        if (c === "{") depth++;
        else if (c === "}" && depth > 0) depth--;
        else if (c === "," && depth === 0) {
          alternatives.push(pattern.slice(alternativeStart, i));
          alternativeStart = i + 1;
        } else if (c === "}" && depth === 0) {
          if (alternatives.length > 0) {
            alternatives.push(pattern.slice(alternativeStart, i));
            expression += `(?:${alternatives.map(baseline_globFragmentToRegExp).join("|")})`;
            index = i;
            matched = true;
          }
          break;
        }
      }
      if (!matched) {
        expression += "\\{";
      }
    } else {
      expression += char.replace(/[|\\{}()[\]^$+*?.]/g, "\\$&");
    }
  }
  return expression;
}

function baseline_globToRegExp(pattern, caseInsensitive = false) {
  let expression = pattern.includes("/") ? "^" : "^(?:.*/)?";
  expression += baseline_globFragmentToRegExp(pattern);
  return new RegExp(`${expression}$`, caseInsensitive ? "i" : undefined);
}

function baseline_globPatternMatches(pattern, path, caseInsensitive) {
  if (pattern.endsWith("/**")) {
    const directoryPattern = pattern.slice(0, -3);
    if (baseline_globToRegExp(directoryPattern, caseInsensitive).test(path)) {
      return true;
    }
  }
  return baseline_globToRegExp(pattern, caseInsensitive).test(path);
}

function baseline_pathPatternMatchesWithCase(pattern, path, caseInsensitive) {
  const normalizedPattern = normalizePathPattern(pattern);
  const normalizedPath = normalizePathForMatch(path);
  if (normalizedPattern.length === 0) return false;
  if (hasPathGlob(normalizedPattern)) {
    return baseline_globPatternMatches(normalizedPattern, normalizedPath, caseInsensitive);
  }
  const candidate = caseInsensitive ? normalizedPath.toLowerCase() : normalizedPath;
  const expected = caseInsensitive ? normalizedPattern.toLowerCase() : normalizedPattern;
  const expectedPrefix = expected.endsWith("/") ? expected : `${expected}/`;
  return candidate === expected || candidate.startsWith(expectedPrefix);
}

function baseline_pathPatternMatches(pattern, path) {
  return baseline_pathPatternMatchesWithCase(pattern, path, false);
}

function baseline_ripgrepGlobMatches(pattern, path) {
  const normalizedPattern = normalizePathPattern(pattern);
  if (normalizedPattern.length === 0) return false;
  return baseline_globPatternMatches(normalizedPattern, normalizePathForMatch(path), false);
}

function baseline_literalPrefixBeforeFirstGlob(pattern) {
  const indexes = [pattern.indexOf("*"), pattern.indexOf("?")].filter(
    (index) => index >= 0,
  );
  if (indexes.length === 0) {
    return pattern;
  }
  return pattern.slice(0, Math.min(...indexes));
}

function baseline_patternPrefixMightMatchDescendant(pattern, directoryPath) {
  const directoryPrefix = `${directoryPath}/`;
  const variants = pattern.startsWith("**/")
    ? [pattern, pattern.slice(3)]
    : [pattern];

  for (const variant of variants) {
    if (!hasPathGlob(variant)) {
      if (variant.startsWith(directoryPrefix)) {
        return true;
      }
      continue;
    }

    const literalPrefix = baseline_literalPrefixBeforeFirstGlob(variant);
    if (
      literalPrefix.length > 0 &&
      (literalPrefix.startsWith(directoryPrefix) ||
        directoryPrefix.startsWith(literalPrefix))
    ) {
      return true;
    }
  }

  return false;
}

function baseline_pathPatternMightMatchDescendant(pattern, directoryPath) {
  const normalizedPattern = normalizePathPattern(pattern);
  const normalizedDirectory = normalizePathForMatch(directoryPath).replace(/\/+$/, "");
  if (normalizedDirectory.length === 0) return true;
  return (
    baseline_pathPatternMatches(pattern, normalizedDirectory) ||
    baseline_pathPatternMatches(pattern, `${normalizedDirectory}/__zvec_grep_descendant__`) ||
    baseline_patternPrefixMightMatchDescendant(normalizedPattern, normalizedDirectory)
  );
}

// ============================================================================
// 2. Real Workspace Path Collection
// ============================================================================

function collectWorkspaceFiles(dir, rootDir = dir) {
  const files = [];
  try {
    const entries = readdirSync(dir, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.name === ".git" || entry.name === "node_modules") {
        continue;
      }
      const fullPath = join(dir, entry.name);
      const relPath = normalizePathForMatch(relative(rootDir, fullPath));
      files.push(relPath);
      if (entry.isDirectory()) {
        files.push(...collectWorkspaceFiles(fullPath, rootDir));
      }
    }
  } catch (err) {
    // Ignore unreadable dirs
  }
  return files;
}

const workspaceRoot = process.cwd();
const realPaths = collectWorkspaceFiles(workspaceRoot);

console.log(`[Benchmark] Collected ${realPaths.length} real workspace paths.`);

// Realistic pattern set from zvec-grep defaults and standard user queries
const testPatterns = [
  "*.lock",
  "*.lockb",
  "*-lock.json",
  "*-lock.yaml",
  "npm-shrinkwrap.json",
  "go.sum",
  "*.resolved",
  "*.po",
  "*.pot",
  "*.map",
  "*.min.*",
  "*.bundle.*",
  "*.generated.*",
  "*.gen.*",
  "*.designer.*",
  "*.pb.*",
  "*_pb2.*",
  "*.g.*",
  "*.gif",
  "*.jpeg",
  "*.jpg",
  "*.png",
  "*.webp",
  "node_modules",
  "vendor",
  "dist",
  "dist/**",
  "build",
  "src/**/*.ts",
  "src/engine/**/*.ts",
  "test/**/*.test.mjs",
  "*.ts",
  "*.json",
  "*.md",
  "docs/**",
  "{src,test}/**/*.ts",
];

const ITERATIONS = 100;
const totalEvaluations = ITERATIONS * realPaths.length * testPatterns.length;

console.log(`[Benchmark] Testing ${ITERATIONS} iterations across ${realPaths.length} paths x ${testPatterns.length} patterns (${totalEvaluations} evals/test)...`);

// ============================================================================
// 3. Correctness & Parity Assertion
// ============================================================================
console.log("[Benchmark] Verifying 100% output parity between baseline and optimized implementations...");
for (const path of realPaths) {
  for (const pattern of testPatterns) {
    const baselinePM = baseline_pathPatternMatches(pattern, path);
    const optimizedPM = optimized_pathPatternMatches(pattern, path);
    assert.equal(
      optimizedPM,
      baselinePM,
      `Mismatch in pathPatternMatches for pattern "${pattern}" on path "${path}"`
    );

    const baselineRG = baseline_ripgrepGlobMatches(pattern, path);
    const optimizedRG = optimized_ripgrepGlobMatches(pattern, path);
    assert.equal(
      optimizedRG,
      baselineRG,
      `Mismatch in ripgrepGlobMatches for pattern "${pattern}" on path "${path}"`
    );

    const baselineDesc = baseline_pathPatternMightMatchDescendant(pattern, path);
    const optimizedDesc = optimized_pathPatternMightMatchDescendant(pattern, path);
    assert.equal(
      optimizedDesc,
      baselineDesc,
      `Mismatch in pathPatternMightMatchDescendant for pattern "${pattern}" on path "${path}"`
    );
  }
}
console.log("✅ Parity verified: All outputs are 100% identical across all 3 functions.\n");

// ============================================================================
// 4. Warmup
// ============================================================================
for (let i = 0; i < 5; i++) {
  for (const path of realPaths.slice(0, 50)) {
    for (const pattern of testPatterns.slice(0, 10)) {
      baseline_pathPatternMatches(pattern, path);
      optimized_pathPatternMatches(pattern, path);
      baseline_ripgrepGlobMatches(pattern, path);
      optimized_ripgrepGlobMatches(pattern, path);
      baseline_pathPatternMightMatchDescendant(pattern, path);
      optimized_pathPatternMightMatchDescendant(pattern, path);
    }
  }
}

// ============================================================================
// 5. Benchmark Execution
// ============================================================================

// Suite 1: pathPatternMatches
let baselinePMCount = 0;
const t0_b_pm = performance.now();
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (baseline_pathPatternMatches(pattern, path)) baselinePMCount++;
    }
  }
}
const baseline_pm_ms = performance.now() - t0_b_pm;

let optimizedPMCount = 0;
const t0_o_pm = performance.now();
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (optimized_pathPatternMatches(pattern, path)) optimizedPMCount++;
    }
  }
}
const optimized_pm_ms = performance.now() - t0_o_pm;

// Suite 2: ripgrepGlobMatches
let baselineRGCount = 0;
const t0_b_rg = performance.now();
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (baseline_ripgrepGlobMatches(pattern, path)) baselineRGCount++;
    }
  }
}
const baseline_rg_ms = performance.now() - t0_b_rg;

let optimizedRGCount = 0;
const t0_o_rg = performance.now();
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (optimized_ripgrepGlobMatches(pattern, path)) optimizedRGCount++;
    }
  }
}
const optimized_rg_ms = performance.now() - t0_o_rg;

// Suite 3: pathPatternMightMatchDescendant
let baselineDescCount = 0;
const t0_b_desc = performance.now();
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (baseline_pathPatternMightMatchDescendant(pattern, path)) baselineDescCount++;
    }
  }
}
const baseline_desc_ms = performance.now() - t0_b_desc;

let optimizedDescCount = 0;
const t0_o_desc = performance.now();
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (optimized_pathPatternMightMatchDescendant(pattern, path)) optimizedDescCount++;
    }
  }
}
const optimized_desc_ms = performance.now() - t0_o_desc;

// ============================================================================
// 6. Report Comparison
// ============================================================================

function reportSuite(name, baselineMs, optimizedMs, bCount, oCount) {
  assert.equal(bCount, oCount, `Match count mismatch in ${name}!`);
  const speedup = (baselineMs / optimizedMs).toFixed(1);
  const reduction = (((baselineMs - optimizedMs) / baselineMs) * 100).toFixed(1);
  console.log(`⚡ ${name}:`);
  console.log(`   Baseline:  ${baselineMs.toFixed(2)} ms`);
  console.log(`   Optimized: ${optimizedMs.toFixed(2)} ms`);
  console.log(`   Speedup:   ${speedup}x faster (${reduction}% reduction)\n`);
}

const totalBaseline = baseline_pm_ms + baseline_rg_ms + baseline_desc_ms;
const totalOptimized = optimized_pm_ms + optimized_rg_ms + optimized_desc_ms;
const totalSpeedup = (totalBaseline / totalOptimized).toFixed(1);
const totalReduction = (((totalBaseline - totalOptimized) / totalBaseline) * 100).toFixed(1);

console.log("==================================================");
console.log(`🏆 BENCHMARK RESULTS (${totalEvaluations} evaluations per suite)`);
console.log("==================================================");
reportSuite("1. pathPatternMatches", baseline_pm_ms, optimized_pm_ms, baselinePMCount, optimizedPMCount);
reportSuite("2. ripgrepGlobMatches", baseline_rg_ms, optimized_rg_ms, baselineRGCount, optimizedRGCount);
reportSuite("3. pathPatternMightMatchDescendant", baseline_desc_ms, optimized_desc_ms, baselineDescCount, optimizedDescCount);
console.log("--------------------------------------------------");
console.log(`TOTAL BENCHMARK DURATION:`);
console.log(`   Baseline:  ${totalBaseline.toFixed(2)} ms (~${(totalBaseline / 1000).toFixed(2)}s)`);
console.log(`   Optimized: ${totalOptimized.toFixed(2)} ms (~${(totalOptimized / 1000).toFixed(2)}s)`);
console.log(`🚀 OVERALL SPEEDUP: ${totalSpeedup}x faster (${totalReduction}% reduction in execution time)`);
console.log("==================================================");
