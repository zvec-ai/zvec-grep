import { realpathSync } from "node:fs";
import { sep } from "node:path";

// Hit paths carry the platform's native separators (Windows "\"), while
// the expected relative suffixes and containment checks in the
// integration tests are written with "/". Both separator styles are
// normalized so the Windows regression is verifiable from any platform.
export function toPortableSeparators(path) {
  return path.split(/[\\/]/).join("/");
}

// Segment-aligned suffix match for an expected relative file path
// against a hit path in native separators. Replaces bare
// `filePath.endsWith("docs/renamed.md")`, which never matches a
// backslash-separated hit path (hosted Windows failures,
// CI run 36778689252).
export function endsWithRelative(filePath, relativeSuffix) {
  const portable = toPortableSeparators(filePath);
  const suffix = toPortableSeparators(relativeSuffix);
  return portable === suffix || portable.endsWith(`/${suffix}`);
}

// Physical containment of a hit path under a workspace root: both sides
// are resolved (macOS /var vs /private/var, Windows short names) and
// compared with the native separator.
export function physicallyUnder(filePath, root) {
  const realFile = realpathSync(filePath);
  const realRoot = realpathSync(root);
  return realFile === realRoot || realFile.startsWith(realRoot + sep);
}
