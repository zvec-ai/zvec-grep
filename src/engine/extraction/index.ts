export { analyzeForIndexing, extract, extractForIndexing } from "./runtime.js";
export type { PreparedCodeAnalysis } from "./runtime.js";
export type { Source, TextSource } from "./source.js";
export {
  collectFunctionCallSites,
  collectSymbolRefSites,
  collectTypeInheritanceSites,
} from "./code/extractor.js";
export { collectImportSpecs, type ImportSpec } from "./code/import-sites.js";
export type { ChunkOptions } from "./types.js";
export { vectorContentForFragment } from "./vector-content.js";
