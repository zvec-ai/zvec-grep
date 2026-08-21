import { resolveAdapter } from "./adapter.js";
import type { TextSource } from "../source.js";
import { hasGrammar } from "./tree-sitter/grammar.js";
import type { TSNode } from "./tree-sitter/nodes.js";
import { withParser } from "./tree-sitter/parser.js";

export type ImportSpec = {
  spec: string;
  line: number;
  bindings?: readonly ImportBinding[];
  /** true when #include <...> — always treated as external in v1 */
  systemInclude?: boolean;
};

export type ImportBinding = { imported: string; local: string };

const IMPORT_NODE_TYPES = new Set([
  "import_statement",
  "import_from_statement",
  "export_statement",
  "preproc_include",
]);

/**
 * Collect import/include module specs from a source file (inside withParser).
 * Drops obvious externals early; path resolution happens in resolvePending.
 */
export async function collectImportSpecs(
  source: TextSource,
): Promise<readonly ImportSpec[]> {
  if (source.file.kind !== "code" || !hasGrammar(source.file.format)) {
    return [];
  }
  if (!resolveAdapter(source.file.format)) {
    return [];
  }

  const language = source.file.format;
  const collected = await withParser(source.text, language, (tree) => {
    return collectImportSpecsFromNode(tree.rootNode, language);
  });

  return collected ?? [];
}

export function collectImportSpecsFromNode(
  root: TSNode,
  language: string,
): ImportSpec[] {
  const out: ImportSpec[] = [];
  const seen = new Set<string>();
  const visit = (node: TSNode): void => {
    if (IMPORT_NODE_TYPES.has(node.type)) {
      for (const spec of extractSpecsFromNode(node, language)) {
        if (spec.systemInclude) continue;
        const key = `${spec.spec}\0${spec.line}`;
        if (!seen.has(key)) {
          seen.add(key);
          out.push(spec);
        }
      }
    }
    for (const child of node.namedChildren ?? []) visit(child);
  };
  visit(root);
  return out;
}

function extractSpecsFromNode(node: TSNode, language: string): ImportSpec[] {
  const line = node.startPosition.row + 1;

  if (node.type === "preproc_include") {
    const text = node.text.trim();
    const angle = text.match(/#\s*include\s*<([^>]+)>/);
    if (angle?.[1]) {
      return [{ spec: angle[1], line, systemInclude: true }];
    }
    const quoted = text.match(/#\s*include\s*"([^"]+)"/);
    if (quoted?.[1]) {
      return [{ spec: quoted[1], line }];
    }
    return [];
  }

  if (language === "python" && node.type === "import_from_statement") {
    const module = node.childForFieldName("module_name");
    if (!module) {
      // `from . import x` — module_name may be missing; use leading dots from text
      const match = node.text.match(/^from\s+(\.+[\w.]*)/);
      if (match?.[1]) {
        return [
          {
            spec: match[1],
            line,
            bindings: pythonFromBindings(node, null),
          },
        ];
      }
      return [];
    }
    return [
      {
        spec: module.text.trim(),
        line,
        bindings: pythonFromBindings(node, module),
      },
    ];
  }

  if (language === "python" && node.type === "import_statement") {
    // `import a.b` / `import a, b` — take dotted names
    const specs: ImportSpec[] = [];
    for (const child of node.namedChildren) {
      if (
        child.type === "dotted_name" ||
        child.type === "aliased_import" ||
        child.type === "identifier"
      ) {
        const name =
          child.type === "aliased_import"
            ? (child.childForFieldName("name")?.text ?? child.text)
            : child.text;
        const spec = name.trim();
        if (spec) {
          specs.push({ spec, line });
        }
      }
    }
    return specs;
  }

  // JS/TS import_statement / export_statement … from '…'
  if (node.type === "import_statement" || node.type === "export_statement") {
    const sourceNode = node.childForFieldName("source");
    if (!sourceNode) {
      return [];
    }
    const spec = stripQuotes(sourceNode.text);
    return spec
      ? [{ spec, line, bindings: javascriptNamedBindings(node) }]
      : [];
  }

  return [];
}

function javascriptNamedBindings(node: TSNode): ImportBinding[] {
  const named = descendantsOfType(node, "import_specifier").flatMap(
    (specifier) => {
      const importedNode =
        specifier.childForFieldName("name") ?? specifier.namedChildren[0];
      const localNode =
        specifier.childForFieldName("alias") ?? specifier.namedChildren[1];
      const imported = importedNode?.text.replace(/^type\s+/, "").trim();
      const local = (localNode?.text ?? imported)?.trim();
      return imported && local ? [{ imported, local }] : [];
    },
  );
  const namespaces = descendantsOfType(node, "namespace_import").flatMap(
    (namespace) => {
      const localNode =
        namespace.childForFieldName("alias") ??
        namespace.namedChildren.find((child) => child.type === "identifier");
      const local = localNode?.text.trim();
      return local ? [{ imported: "*", local }] : [];
    },
  );
  return [...named, ...namespaces];
}

function pythonFromBindings(
  node: TSNode,
  moduleNode: TSNode | null,
): ImportBinding[] {
  const bindings: ImportBinding[] = [];
  const visit = (current: TSNode): void => {
    if (moduleNode && sameNode(current, moduleNode)) return;
    if (current.type === "aliased_import") {
      const importedNode =
        current.childForFieldName("name") ?? current.namedChildren[0];
      const localNode =
        current.childForFieldName("alias") ?? current.namedChildren[1];
      const imported = importedNode?.text.trim();
      const local = localNode?.text.trim();
      if (imported && local) bindings.push({ imported, local });
      return;
    }
    if (
      current !== node &&
      (current.type === "identifier" || current.type === "dotted_name")
    ) {
      const name = current.text.trim();
      if (name) bindings.push({ imported: name, local: name });
      return;
    }
    for (const child of current.namedChildren ?? []) visit(child);
  };
  visit(node);
  return bindings;
}

function sameNode(left: TSNode, right: TSNode): boolean {
  return (
    left.type === right.type &&
    left.startIndex === right.startIndex &&
    left.endIndex === right.endIndex
  );
}

function descendantsOfType(node: TSNode, type: string): TSNode[] {
  const out: TSNode[] = [];
  const visit = (current: TSNode): void => {
    if (current.type === type) {
      out.push(current);
      return;
    }
    for (const child of current.namedChildren ?? []) visit(child);
  };
  visit(node);
  return out;
}

function stripQuotes(value: string): string {
  const trimmed = value.trim();
  if (
    (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
    (trimmed.startsWith("'") && trimmed.endsWith("'")) ||
    (trimmed.startsWith("`") && trimmed.endsWith("`"))
  ) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}
