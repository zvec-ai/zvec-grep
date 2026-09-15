import { homedir } from "node:os";
import { isAbsolute, resolve, sep, win32 } from "node:path";

const WINDOWS_DRIVE_ABSOLUTE = /^[A-Za-z]:[\\/]/;
const CONCATENATED_WINDOWS_DRIVE = /\/[A-Za-z]:[\\/]/;

export function defaultHome(): string {
  return process.env.ZVEC_GREP_HOME ?? resolve(homedir(), ".zvec-grep");
}

export function isWindowsAbsolutePath(path: string): boolean {
  return WINDOWS_DRIVE_ABSOLUTE.test(path);
}

export function hasConcatenatedWindowsDrive(path: string): boolean {
  return CONCATENATED_WINDOWS_DRIVE.test(path);
}

export function isHostAbsolutePath(path: string): boolean {
  return isAbsolute(path) || isWindowsAbsolutePath(path);
}

export function normalizePath(path: string): string {
  if (process.platform !== "win32" && isWindowsAbsolutePath(path)) {
    return win32.normalize(path);
  }
  return resolve(path);
}

export function resolvePath(from: string, to: string): string {
  if (process.platform !== "win32" && isWindowsAbsolutePath(to)) {
    return win32.normalize(to);
  }
  return resolve(from, to);
}

export function toDisplayPath(path: string): string {
  return path.split(sep).join("/");
}

export function isPathInside(parent: string, child: string): boolean {
  const normalizedParent = normalizePath(parent);
  const normalizedChild = normalizePath(child);

  if (
    process.platform !== "win32" &&
    hasConcatenatedWindowsDrive(normalizedChild) &&
    !hasConcatenatedWindowsDrive(normalizedParent)
  ) {
    return false;
  }

  return (
    normalizedChild === normalizedParent ||
    normalizedChild.startsWith(`${normalizedParent}${sep}`)
  );
}
