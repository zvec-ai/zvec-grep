/** Minimal builtin / common third-party names dropped at resolve time. */

const JS_BUILTINS = new Set([
  "Array",
  "Boolean",
  "Date",
  "Error",
  "JSON",
  "Map",
  "Math",
  "Number",
  "Object",
  "Promise",
  "Proxy",
  "Reflect",
  "RegExp",
  "Set",
  "String",
  "Symbol",
  "WeakMap",
  "WeakSet",
  "console",
  "decodeURI",
  "decodeURIComponent",
  "encodeURI",
  "encodeURIComponent",
  "eval",
  "fetch",
  "isFinite",
  "isNaN",
  "parseFloat",
  "parseInt",
  "setTimeout",
  "clearTimeout",
  "setInterval",
  "clearInterval",
  "require",
  "module",
  "exports",
  "process",
  "Buffer",
  "undefined",
  "NaN",
  "Infinity",
]);

const PYTHON_BUILTINS = new Set([
  "abs",
  "all",
  "any",
  "dict",
  "enumerate",
  "filter",
  "float",
  "format",
  "getattr",
  "hasattr",
  "int",
  "isinstance",
  "len",
  "list",
  "map",
  "max",
  "min",
  "open",
  "print",
  "range",
  "set",
  "sorted",
  "str",
  "sum",
  "super",
  "tuple",
  "type",
  "zip",
]);

const COMMON_PACKAGES = new Set([
  "lodash",
  "react",
  "react-dom",
  "vue",
  "angular",
  "express",
  "fs",
  "path",
  "os",
  "util",
  "http",
  "https",
  "url",
  "crypto",
  "assert",
  "child_process",
  "stream",
  "events",
  "buffer",
  "node:fs",
  "node:path",
  "node:os",
  "node:util",
  "node:http",
  "node:crypto",
]);

export function isExternalRefName(refName: string, language?: string): boolean {
  const bare = bareName(refName);
  if (!bare) {
    return false;
  }
  const root = refName.split(/[./]/)[0] ?? bare;
  const builtins =
    language === "python"
      ? PYTHON_BUILTINS
      : language &&
          !["javascript", "jsx", "typescript", "tsx"].includes(language)
        ? new Set<string>()
        : JS_BUILTINS;
  if (builtins.has(bare) || builtins.has(root)) {
    return true;
  }
  return COMMON_PACKAGES.has(root) || COMMON_PACKAGES.has(bare);
}

export function bareName(refName: string): string {
  const trimmed = refName.trim();
  if (!trimmed) {
    return "";
  }
  const parts = trimmed.split(".");
  return parts[parts.length - 1] ?? trimmed;
}
