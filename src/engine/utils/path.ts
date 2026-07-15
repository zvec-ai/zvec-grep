import { homedir } from "node:os";
import { resolve, sep } from "node:path";

export function defaultHome(): string {
  return process.env.ZVEC_GREP_HOME ?? resolve(homedir(), ".zvec-grep");
}

export function normalizePath(path: string): string {
  return resolve(path);
}

export function toDisplayPath(path: string): string {
  return path.split(sep).join("/");
}

export function isPathInside(parent: string, child: string): boolean {
  const normalizedParent = normalizePath(parent);
  const normalizedChild = normalizePath(child);

  return (
    normalizedChild === normalizedParent ||
    normalizedChild.startsWith(`${normalizedParent}${sep}`)
  );
}
