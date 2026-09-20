import { readFileSync } from "node:fs";

export function readPackageVersion(): string {
  try {
    const packageJson = JSON.parse(
      readFileSync(new URL("../../package.json", import.meta.url), "utf8"),
    ) as { version?: unknown };

    return typeof packageJson.version === "string"
      ? packageJson.version
      : "0.0.0";
  } catch {
    return "0.0.0";
  }
}
