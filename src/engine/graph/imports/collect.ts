import {
  collectImportSpecs as collectSourceImportSpecs,
  type ImportSpec,
  type TextSource,
} from "../../extraction/index.js";
import { isExternalImportSpec } from "./resolve-path.js";

export type { ImportSpec };

/** Collect repository-local imports; package and standard-library imports are excluded. */
export async function collectImportSpecs(
  source: TextSource,
): Promise<readonly ImportSpec[]> {
  const specs = await collectSourceImportSpecs(source);
  return specs.filter(
    (spec) => !isExternalImportSpec(spec.spec, source.file.format),
  );
}
