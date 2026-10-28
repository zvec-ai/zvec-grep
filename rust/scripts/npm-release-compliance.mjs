import { cp, stat } from "node:fs/promises";
import { join } from "node:path";

export async function stageCompliance(source, destination) {
  // Supplemental license texts and SBOMs are optional for both candidates and
  // publication. Preserve either input when supplied without requiring the other.
  for (const name of ["LICENSES", "SBOM.spdx.json"]) {
    const path = join(source, name);
    const exists = await stat(path).catch((error) => {
      if (error.code === "ENOENT") return undefined;
      throw error;
    });
    if (exists) await cp(path, join(destination, name), { recursive: true });
  }
}
