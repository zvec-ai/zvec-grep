// Independent of the native index format: old fragments remain readable while
// ordinary refresh incrementally replaces code files with the current extractor.
// Revisions are per format so a language-specific improvement does not force
// unchanged files in every other language through embedding again.
const JAVASCRIPT_TYPESCRIPT_FORMATS = new Set([
  "javascript",
  "jsx",
  "typescript",
  "tsx",
  "vue",
  "svelte",
]);

export function currentCodeExtractionVersion(format: string): number {
  // 1: complete source coverage; 2: JS/TS function-valued assignments.
  return JAVASCRIPT_TYPESCRIPT_FORMATS.has(format) ? 2 : 1;
}
