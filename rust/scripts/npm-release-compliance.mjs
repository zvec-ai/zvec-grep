import { cp, readFile, readdir, stat } from "node:fs/promises";
import { join } from "node:path";
import { run } from "./npm-package-core.mjs";

function validateCompliance(licenses, sbom, label) {
  if (licenses.length === 0 || licenses.some((text) => !text.trim())) {
    throw new Error(
      `npm publication disabled: missing license texts for ${label}`,
    );
  }
  if (
    sbom.spdxVersion !== "SPDX-2.3" ||
    sbom.dataLicense !== "CC0-1.0" ||
    sbom.SPDXID !== "SPDXRef-DOCUMENT" ||
    !sbom.documentNamespace ||
    !sbom.creationInfo?.creators?.length ||
    !sbom.creationInfo?.created ||
    !sbom.packages?.length
  ) {
    throw new Error(`npm publication disabled: invalid SPDX SBOM for ${label}`);
  }
}

async function licenseTexts(directory) {
  const texts = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) texts.push(...(await licenseTexts(path)));
    else if (entry.isFile()) texts.push(await readFile(path, "utf8"));
  }
  return texts;
}

export async function verifyComplianceDirectory(directory) {
  try {
    const licenses = await licenseTexts(join(directory, "LICENSES"));
    const sbom = JSON.parse(
      await readFile(join(directory, "SBOM.spdx.json"), "utf8"),
    );
    validateCompliance(licenses, sbom, directory);
  } catch (error) {
    throw new Error(
      `npm publication disabled: incomplete compliance files in ${directory}`,
      { cause: error },
    );
  }
}

export async function verifyPublicationInputs(manifest, root) {
  for (const platform of manifest.platforms) {
    await verifyComplianceDirectory(join(root, platform.target));
    for (const component of platform.components ?? []) {
      await verifyComplianceDirectory(join(root, platform.target,
        component.package.slice(platform.package.length + 1)));
    }
  }
}

export async function stageCompliance(source, destination) {
  const exists = await stat(source).catch((error) => {
    if (error.code === "ENOENT") return undefined;
    throw error;
  });
  // Downloadable candidates remain available while reviewed compliance inputs
  // are being prepared. Registry publication independently checks every tarball.
  if (!exists) return;
  await verifyComplianceDirectory(source);
  for (const name of ["LICENSES", "SBOM.spdx.json"]) {
    await cp(join(source, name), join(destination, name), { recursive: true });
  }
}

export function verifyTarballCompliance(path) {
  const files = run("tar", ["-tf", path], { capture: true })
    .trim()
    .split(/\r?\n/);
  const licenses = files.filter(
    (name) => name.startsWith("package/LICENSES/") && !name.endsWith("/"),
  );
  if (licenses.length === 0 || !files.includes("package/SBOM.spdx.json")) {
    throw new Error(
      `npm publication disabled: LICENSES and SBOM.spdx.json are required in ${path}`,
    );
  }
  const texts = licenses.map((name) =>
    run("tar", ["-xOf", path, name], { capture: true }),
  );
  const sbom = JSON.parse(
    run("tar", ["-xOf", path, "package/SBOM.spdx.json"], { capture: true }),
  );
  validateCompliance(texts, sbom, path);
}
