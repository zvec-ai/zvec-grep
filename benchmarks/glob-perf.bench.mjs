import { readdirSync } from "node:fs";
import { join, relative } from "node:path";
import { performance } from "node:perf_hooks";
import {
  pathPatternMatches,
  ripgrepGlobMatches,
  pathPatternMightMatchDescendant,
  normalizePathForMatch,
} from "../dist/engine/utils/glob.js";

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

console.log(`[Benchmark] Running ${ITERATIONS} iterations across ${realPaths.length} paths x ${testPatterns.length} patterns (${ITERATIONS * realPaths.length * testPatterns.length} evaluations)...`);

// Warmup
for (let i = 0; i < 5; i++) {
  for (const path of realPaths.slice(0, 50)) {
    for (const pattern of testPatterns.slice(0, 10)) {
      pathPatternMatches(pattern, path);
      ripgrepGlobMatches(pattern, path);
    }
  }
}

// Measure pathPatternMatches
const startPathMatches = performance.now();
let matchCount = 0;
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (pathPatternMatches(pattern, path)) {
        matchCount++;
      }
    }
  }
}
const endPathMatches = performance.now();
const pathMatchesDuration = endPathMatches - startPathMatches;

// Measure ripgrepGlobMatches
const startRipgrepMatches = performance.now();
let rgMatchCount = 0;
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (ripgrepGlobMatches(pattern, path)) {
        rgMatchCount++;
      }
    }
  }
}
const endRipgrepMatches = performance.now();
const ripgrepMatchesDuration = endRipgrepMatches - startRipgrepMatches;

// Measure pathPatternMightMatchDescendant
const startDescendant = performance.now();
let descendantMatchCount = 0;
for (let iter = 0; iter < ITERATIONS; iter++) {
  for (const path of realPaths) {
    for (const pattern of testPatterns) {
      if (pathPatternMightMatchDescendant(pattern, path)) {
        descendantMatchCount++;
      }
    }
  }
}
const endDescendant = performance.now();
const descendantDuration = endDescendant - startDescendant;

const totalOps = ITERATIONS * realPaths.length * testPatterns.length;

console.log("--------------------------------------------------");
console.log(`Total pattern evaluations per suite: ${totalOps}`);
console.log(`1. pathPatternMatches: ${pathMatchesDuration.toFixed(2)} ms (matches: ${matchCount})`);
console.log(`2. ripgrepGlobMatches: ${ripgrepMatchesDuration.toFixed(2)} ms (matches: ${rgMatchCount})`);
console.log(`3. pathPatternMightMatchDescendant: ${descendantDuration.toFixed(2)} ms (matches: ${descendantMatchCount})`);
console.log(`Total Duration: ${(pathMatchesDuration + ripgrepMatchesDuration + descendantDuration).toFixed(2)} ms`);
console.log("--------------------------------------------------");
