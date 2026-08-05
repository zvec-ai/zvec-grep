import { EngineError } from "../../errors/index.js";
import type {
  CodeEntityMetadata,
  CodeEntityModifier,
  CodeSymbolType,
  EntityFragment,
  TextRange,
} from "../../types.js";
import { makeEntityId } from "../ids.js";
import type { ChunkOptions, Extractor } from "../index.js";
import { validateSourceFile, type Source, type TextSource } from "../source.js";
import { hasGrammar } from "../tree-sitter/grammar.js";
import type { TSNode } from "../tree-sitter/nodes.js";
import { withParser } from "../tree-sitter/parser.js";
import { resolveAdapter, type LanguageAdapter } from "./adapter.js";
import { hasJavascriptTypescriptFunctionValue } from "./families/js-ts.js";

const DEFAULT_CODE_CHUNK_CHARS = 3600;
const DEFAULT_CODE_CHUNK_OVERLAP_CHARS = 540;

export class CodeExtractor implements Extractor {
  private readonly maxChunkChars: number;
  private readonly chunkOverlapChars: number;

  constructor(options: ChunkOptions = {}) {
    const maxChunkChars = options.maxChunkChars ?? DEFAULT_CODE_CHUNK_CHARS;
    const chunkOverlapChars =
      options.chunkOverlapChars ?? DEFAULT_CODE_CHUNK_OVERLAP_CHARS;

    if (!Number.isInteger(maxChunkChars) || maxChunkChars <= 0) {
      throw new EngineError(
        "Code extractor requires a positive integer chunk size",
        {
          code: "ZVEC_GREP.ENGINE.EXTRACTORS.CODE_INVALID_CHUNK_SIZE",
          context: `maxChunkChars=${maxChunkChars}`,
        },
      );
    }

    if (
      !Number.isInteger(chunkOverlapChars) ||
      chunkOverlapChars < 0 ||
      chunkOverlapChars >= maxChunkChars
    ) {
      throw new EngineError(
        "Code extractor requires overlap to be smaller than chunk size",
        {
          code: "ZVEC_GREP.ENGINE.EXTRACTORS.CODE_INVALID_CHUNK_OVERLAP",
          context: `maxChunkChars=${maxChunkChars} chunkOverlapChars=${chunkOverlapChars}`,
        },
      );
    }

    this.maxChunkChars = maxChunkChars;
    this.chunkOverlapChars = chunkOverlapChars;
  }

  supports(source: Source): boolean {
    return (
      source.kind === "text" &&
      source.file.kind === "code" &&
      (isScriptBlockFormat(source.file.format) ||
        (hasGrammar(source.file.format) &&
          resolveAdapter(source.file.format) !== null))
    );
  }

  async extract(source: Source): Promise<EntityFragment[]> {
    if (!this.supports(source) || source.kind !== "text") {
      return [];
    }

    validateSourceFile(source);

    if (isScriptBlockFormat(source.file.format)) {
      return this.extractScriptBlocks(source);
    }

    const adapter = resolveAdapter(source.file.format);
    if (!adapter) {
      return [];
    }

    const extracted = await withParser(
      source.text,
      source.file.format,
      (tree) => {
        const collected: CodeEntity[] = [];
        walkCodeNode(tree.rootNode, adapter, [], collected);

        const output: EntityFragment[] = [];
        let entityIdIndex = 0;
        const appendEntity = (entity: CodeEntity): void => {
          const fragments = codeEntityToSearchFragments(
            source,
            adapter,
            entity,
            this.maxChunkChars,
            this.chunkOverlapChars,
          );
          const majorId =
            fragments[0]?.group === ""
              ? makeEntityId(source.file.id, entityIdIndex)
              : null;

          for (const item of fragments) {
            const id = makeEntityId(source.file.id, entityIdIndex);
            output.push({
              id,
              ...item,
              group: item.group === "" ? id : (majorId ?? item.group),
            });
            entityIdIndex++;
          }
        };

        for (const entity of collected) {
          appendEntity(entity);
        }

        return output;
      },
    );

    if (!extracted || extracted.length === 0) {
      return [];
    }

    return extracted;
  }

  private async extractScriptBlocks(
    source: TextSource,
  ): Promise<EntityFragment[]> {
    const fragments: EntityFragment[] = [];

    for (const block of findScriptBlocks(source.text)) {
      const blockFile = {
        ...source.file,
        format: block.format,
      };
      const blockFragments = await this.extract({
        kind: "text",
        file: blockFile,
        text: block.text,
      });

      fragments.push(
        ...remapScriptBlockFragments(
          source.file.id,
          blockFragments,
          fragments.length,
          block.startLine,
          block.startOffset,
        ),
      );
    }

    return fragments;
  }
}

type CodeEntity = {
  node: TSNode;
  name?: string;
  symbolType: CodeSymbolType;
  breadcrumb: readonly string[];
  signature?: string;
  doc?: string;
  modifiers: readonly CodeEntityModifier[];
};

type CodeWindow = {
  text: string;
  range: TextRange;
};

type CodeFragmentOutput = Omit<EntityFragment, "id">;

type ScriptBlock = {
  text: string;
  format: "javascript" | "jsx" | "typescript" | "tsx";
  startLine: number;
  startOffset: number;
};

function walkCodeNode(
  node: TSNode,
  adapter: LanguageAdapter,
  breadcrumb: readonly string[],
  out: CodeEntity[],
): void {
  for (const child of node.children) {
    const isScope =
      adapter.scopeTypes.has(child.type) &&
      adapter.shouldEnterScope?.(child) !== false;
    const isEntity =
      adapter.entityTypes.has(child.type) &&
      adapter.shouldIndexEntity?.(child) !== false;

    if (isEntity) {
      const entities = adapter.resolveEntities?.(child) ?? [
        adapter.resolveEntity ? adapter.resolveEntity(child) : child,
      ];

      for (const entity of entities) {
        const name = adapter.extractName(entity);
        const entityBreadcrumb =
          adapter.scopeBreadcrumb?.(entity, breadcrumb) ?? breadcrumb;
        const symbolType =
          adapter.classifyNode?.(entity, entityBreadcrumb) ??
          classifyCodeNode(entity, entityBreadcrumb);

        out.push({
          node: entity,
          name,
          symbolType,
          breadcrumb: entityBreadcrumb,
          signature: adapter.extractSignature?.(entity),
          doc: adapter.extractDoc?.(entity),
          modifiers: adapter.extractModifiers?.(entity) ?? [],
        });
      }
    }

    if (isScope) {
      const name = adapter.extractName(child);
      const scopeNode = adapter.enterScopeNode?.(child) ?? child;
      walkCodeNode(
        scopeNode,
        adapter,
        name ? [...breadcrumb, name] : breadcrumb,
        out,
      );
      continue;
    }

    if (!isEntity) {
      walkCodeNode(child, adapter, breadcrumb, out);
    }
  }
}

function codeEntityToSearchFragments(
  source: TextSource,
  adapter: LanguageAdapter,
  entity: CodeEntity,
  maxChars: number,
  overlapChars: number,
): CodeFragmentOutput[] {
  if (entity.node.text.length <= maxChars) {
    return [
      codeEntityWindowToFragment(source, entity, nodeToWindow(entity.node)),
    ];
  }

  const metadata = codeEntityMetadata(entity);
  const major: CodeFragmentOutput = {
    group: "",
    fileId: source.file.id,
    range: nodeToWindow(entity.node).range,
    content: {
      kind: "text",
      text: codeEntityOutline(entity, adapter, maxChars),
    },
    metadata,
  };

  return [
    major,
    ...[...splitLargeNode(entity.node, maxChars, overlapChars)].map((window) =>
      codeEntityWindowToFragment(source, entity, window),
    ),
  ];
}

function codeEntityWindowToFragment(
  source: TextSource,
  entity: CodeEntity,
  window: CodeWindow,
): CodeFragmentOutput {
  return {
    fileId: source.file.id,
    range: window.range,
    content: {
      kind: "text",
      text: window.text,
    },
    metadata: codeEntityMetadata(entity),
  };
}

function nodeToWindow(node: TSNode): CodeWindow {
  return {
    text: node.text,
    range: {
      kind: "text",
      startLine: node.startPosition.row + 1,
      endLine: node.endPosition.row + 1,
      startOffset: node.startIndex,
      endOffset: node.endIndex,
    },
  };
}

function* splitLargeNode(
  node: TSNode,
  maxChars: number,
  overlapChars: number,
): Generator<CodeWindow> {
  const body = node.childForFieldName("body") ?? node;
  const statements = body.namedChildren;

  if (statements.length <= 1) {
    yield* splitTextByLines(
      node.text,
      maxChars,
      node.startPosition.row + 1,
      node.startIndex,
      overlapChars,
    );
    return;
  }

  const baseStart = node.startIndex;
  const source = node.text;
  let groupStart = 0;
  let groupChars = 0;

  for (let index = 0; index < statements.length; index++) {
    const statement = statements[index];
    const statementChars = statement.endIndex - statement.startIndex + 1;

    if (statementChars > maxChars) {
      if (index > groupStart) {
        yield sliceStatements(
          source,
          baseStart,
          statements,
          groupStart,
          index - 1,
        );
      }
      yield* splitTextByLines(
        statement.text,
        maxChars,
        statement.startPosition.row + 1,
        statement.startIndex,
        overlapChars,
      );
      groupStart = index + 1;
      groupChars = 0;
      continue;
    }

    if (groupChars + statementChars > maxChars && index > groupStart) {
      yield sliceStatements(
        source,
        baseStart,
        statements,
        groupStart,
        index - 1,
      );
      const overlapStart = computeOverlapStart(
        statements,
        groupStart,
        index - 1,
        overlapChars,
      );
      let candidateStart = overlapStart < index ? overlapStart : index;
      let candidateChars = statementChars;

      for (let previous = index - 1; previous >= candidateStart; previous--) {
        const addedChars =
          statements[previous].endIndex - statements[previous].startIndex + 1;
        if (candidateChars + addedChars > maxChars) {
          candidateStart = previous + 1;
          break;
        }
        candidateChars += addedChars;
      }

      groupStart = candidateStart;
      groupChars = candidateChars;
      continue;
    }

    groupChars += statementChars;
  }

  if (groupStart < statements.length) {
    yield sliceStatements(
      source,
      baseStart,
      statements,
      groupStart,
      statements.length - 1,
    );
  }
}

function sliceStatements(
  source: string,
  baseStart: number,
  statements: readonly TSNode[],
  startIndex: number,
  endIndex: number,
): CodeWindow {
  const start = statements[startIndex].startIndex;
  const end = statements[endIndex].endIndex;

  return {
    text: source.slice(start - baseStart, end - baseStart),
    range: {
      kind: "text",
      startLine: statements[startIndex].startPosition.row + 1,
      endLine: statements[endIndex].endPosition.row + 1,
      startOffset: start,
      endOffset: end,
    },
  };
}

function* splitTextByLines(
  text: string,
  maxChars: number,
  startLine: number,
  startOffset: number,
  overlapChars: number,
): Generator<CodeWindow> {
  const lines = text.split("\n");
  let lineIndex = 0;
  let offset = startOffset;

  while (lineIndex < lines.length) {
    if (lines[lineIndex].length > maxChars) {
      yield* splitLongLineByChars(
        lines[lineIndex],
        maxChars,
        startLine + lineIndex,
        offset,
        overlapChars,
      );
      offset += lines[lineIndex].length + 1;
      lineIndex++;
      continue;
    }

    let endIndex = lineIndex;
    let usedChars = 0;

    while (endIndex < lines.length) {
      const lineLength = lines[endIndex].length + 1;
      if (usedChars + lineLength > maxChars && endIndex > lineIndex) {
        break;
      }
      usedChars += lineLength;
      endIndex++;
    }

    const chunk = lines.slice(lineIndex, endIndex).join("\n");
    yield {
      text: chunk,
      range: {
        kind: "text",
        startLine: startLine + lineIndex,
        endLine: startLine + endIndex - 1,
        startOffset: offset,
        endOffset: offset + chunk.length,
      },
    };

    if (endIndex >= lines.length) {
      break;
    }

    const overlapLines = computeLineOverlap(
      lines,
      lineIndex,
      endIndex,
      overlapChars,
    );
    const nextIndex = endIndex - overlapLines;
    offset += lines.slice(lineIndex, nextIndex).join("\n").length;
    if (nextIndex > lineIndex) {
      offset += 1;
    }
    lineIndex = nextIndex;
  }
}

function* splitLongLineByChars(
  text: string,
  maxChars: number,
  line: number,
  startOffset: number,
  overlapChars: number,
): Generator<CodeWindow> {
  let relativeStart = 0;

  while (relativeStart < text.length) {
    const rawEnd = Math.min(text.length, relativeStart + maxChars);
    const relativeEnd = safeCharacterEnd(text, relativeStart, rawEnd);

    yield {
      text: text.slice(relativeStart, relativeEnd),
      range: {
        kind: "text",
        startLine: line,
        endLine: line,
        startOffset: startOffset + relativeStart,
        endOffset: startOffset + relativeEnd,
      },
    };

    if (relativeEnd >= text.length) {
      break;
    }

    const rawStart = Math.max(relativeStart + 1, relativeEnd - overlapChars);
    relativeStart = safeCharacterStart(text, rawStart);
  }
}

function safeCharacterEnd(text: string, start: number, end: number): number {
  if (
    end > start &&
    end < text.length &&
    isHighSurrogate(text.charCodeAt(end - 1)) &&
    isLowSurrogate(text.charCodeAt(end))
  ) {
    return end - 1 > start ? end - 1 : end + 1;
  }
  return end;
}

function safeCharacterStart(text: string, start: number): number {
  if (
    start > 0 &&
    start < text.length &&
    isHighSurrogate(text.charCodeAt(start - 1)) &&
    isLowSurrogate(text.charCodeAt(start))
  ) {
    return start + 1;
  }
  return start;
}

function isHighSurrogate(value: number): boolean {
  return value >= 0xd800 && value <= 0xdbff;
}

function isLowSurrogate(value: number): boolean {
  return value >= 0xdc00 && value <= 0xdfff;
}

function computeOverlapStart(
  statements: readonly TSNode[],
  groupStart: number,
  groupEnd: number,
  overlapChars: number,
): number {
  if (overlapChars <= 0) {
    return groupEnd + 1;
  }

  let chars = 0;
  let index = groupEnd;

  while (index >= groupStart && chars < overlapChars) {
    chars += statements[index].endIndex - statements[index].startIndex + 1;
    index--;
  }

  return index + 1;
}

function computeLineOverlap(
  lines: readonly string[],
  startIndex: number,
  endIndex: number,
  overlapChars: number,
): number {
  if (overlapChars <= 0) {
    return 0;
  }

  let chars = 0;
  let count = 0;

  for (let index = endIndex - 1; index >= startIndex; index--) {
    chars += lines[index].length + 1;
    if (chars > overlapChars) {
      break;
    }
    count++;
  }

  return Math.min(count, Math.floor((endIndex - startIndex) / 2));
}

const STRUCTURAL_SYMBOL_TYPES = new Set<CodeSymbolType>([
  "class",
  "interface",
  "module",
]);

const OUTLINE_MAX_MEMBERS = 32;
const OUTLINE_MAX_CALLS = 24;
const OUTLINE_MAX_LINE_CHARS = 180;

type OutlineMember = {
  symbolType: CodeSymbolType;
  name?: string;
  signature?: string;
};

function codeEntityOutline(
  entity: CodeEntity,
  adapter: LanguageAdapter,
  maxChars: number,
): string {
  const header = extractCodeHeader(entity.node.text);
  const lines = [
    header.length > 0 ? header : (entity.name ?? entity.symbolType),
  ];

  if (STRUCTURAL_SYMBOL_TYPES.has(entity.symbolType)) {
    const members = collectStructureOutlineMembers(entity, adapter);
    if (members.length > 0) {
      lines.push(
        "",
        "members:",
        ...members.map((member) => `- ${formatOutlineMember(member)}`),
      );
    }
  } else if (entity.symbolType === "function") {
    const calls = collectFunctionCallNames(entity.node);
    if (calls.length > 0) {
      lines.push("", `calls: ${calls.join(", ")}`);
    }
  }

  return truncateOutline(lines.join("\n").trim(), maxChars);
}

function truncateOutline(outline: string, maxChars: number): string {
  if (outline.length <= maxChars) {
    return outline;
  }
  if (maxChars <= 3) {
    return ".".repeat(maxChars);
  }
  return `${outline.slice(0, maxChars - 3).trimEnd()}...`;
}

function extractCodeHeader(text: string): string {
  const maxChars = 1200;
  const maxLines = 24;
  const lines: string[] = [];

  for (const line of text.split(/\r?\n/)) {
    lines.push(line);
    if (line.includes("{") || lines.length >= maxLines) {
      break;
    }
  }

  const header = lines.join("\n").trim();

  return header.length > maxChars
    ? `${header.slice(0, maxChars).trimEnd()}\n...`
    : header;
}

function collectStructureOutlineMembers(
  entity: CodeEntity,
  adapter: LanguageAdapter,
): OutlineMember[] {
  const members: OutlineMember[] = [];
  const seen = new Set<string>();

  const visit = (node: TSNode, depth: number): void => {
    if (members.length >= OUTLINE_MAX_MEMBERS || depth > 12) {
      return;
    }

    if (!sameNode(node, entity.node) && adapter.entityTypes.has(node.type)) {
      if (adapter.shouldIndexEntity?.(node) !== false) {
        const resolved = adapter.resolveEntities?.(node) ?? [
          adapter.resolveEntity ? adapter.resolveEntity(node) : node,
        ];

        for (const resolvedNode of resolved) {
          if (
            members.length >= OUTLINE_MAX_MEMBERS ||
            sameNode(resolvedNode, entity.node)
          ) {
            break;
          }

          const name = adapter.extractName(resolvedNode);
          const symbolType =
            adapter.classifyNode?.(resolvedNode, entity.breadcrumb) ??
            classifyCodeNode(resolvedNode, entity.breadcrumb);
          const signature = adapter.extractSignature?.(resolvedNode);
          const key = `${symbolType}:${name ?? ""}:${signature ?? ""}:${resolvedNode.startIndex}`;

          if (!seen.has(key)) {
            seen.add(key);
            members.push({ symbolType, name, signature });
          }
        }
      }

      return;
    }

    for (const child of node.namedChildren) {
      visit(child, depth + 1);
    }
  };

  visit(entity.node, 0);
  return members;
}

function formatOutlineMember(member: OutlineMember): string {
  const name = member.name ?? "";
  const signature = member.signature
    ? truncateOutlineLine(oneLine(member.signature))
    : "";

  if (signature.length > 0) {
    return name.length > 0 && !signature.includes(name)
      ? `${member.symbolType} ${name}: ${signature}`
      : `${member.symbolType} ${signature}`;
  }

  return name.length > 0 ? `${member.symbolType} ${name}` : member.symbolType;
}

function collectFunctionCallNames(node: TSNode): string[] {
  const calls: string[] = [];
  const seen = new Set<string>();

  const visit = (current: TSNode): void => {
    if (calls.length >= OUTLINE_MAX_CALLS) {
      return;
    }

    if (isCallNode(current)) {
      const name = extractCallName(current);
      if (name && !seen.has(name)) {
        seen.add(name);
        calls.push(name);
      }
    }

    for (const child of current.namedChildren) {
      visit(child);
    }
  };

  visit(node);
  return calls;
}

function isCallNode(node: TSNode): boolean {
  return (
    node.type === "call" ||
    node.type === "call_expression" ||
    node.type === "function_call_expression" ||
    node.type === "method_invocation" ||
    node.type === "object_creation_expression" ||
    node.type === "new_expression"
  );
}

function extractCallName(node: TSNode): string | undefined {
  const target =
    node.childForFieldName("function") ??
    node.childForFieldName("name") ??
    node.childForFieldName("constructor") ??
    node.childForFieldName("type") ??
    node.namedChildren[0];

  if (!target) {
    return undefined;
  }

  return normalizeCallName(target.text);
}

function normalizeCallName(value: string): string | undefined {
  const cleaned = value
    .replace(/\s+/g, " ")
    .replace(/^new\s+/, "")
    .trim();

  if (
    cleaned.length === 0 ||
    cleaned.length > OUTLINE_MAX_LINE_CHARS ||
    /[\n\r]/.test(cleaned) ||
    !/[A-Za-z_$][A-Za-z0-9_$]*/.test(cleaned)
  ) {
    return undefined;
  }

  return cleaned;
}

function sameNode(left: TSNode, right: TSNode): boolean {
  return (
    left.startIndex === right.startIndex &&
    left.endIndex === right.endIndex &&
    left.type === right.type
  );
}

function oneLine(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function truncateOutlineLine(value: string): string {
  return value.length > OUTLINE_MAX_LINE_CHARS
    ? `${value.slice(0, OUTLINE_MAX_LINE_CHARS - 3).trimEnd()}...`
    : value;
}

function codeEntityMetadata(entity: CodeEntity): CodeEntityMetadata {
  return {
    kind: "code",
    symbolType: entity.symbolType,
    symbolName: entity.name ?? null,
    scope: entity.breadcrumb.length > 0 ? entity.breadcrumb.join("::") : null,
    nodeType: entity.node.type,
    signature: entity.signature ?? null,
    doc: entity.doc ?? null,
    modifiers: entity.modifiers,
  };
}

function classifyCodeNode(
  node: TSNode,
  breadcrumb: readonly string[],
): CodeSymbolType {
  const nodeType = node.type;

  if (nodeType === "decorated_definition") {
    const inner = node.namedChildren.find(
      (child) =>
        child.type === "function_definition" ||
        child.type === "class_definition",
    );

    return inner ? classifyCodeNode(inner, breadcrumb) : "value";
  }

  if (
    (nodeType === "field_definition" ||
      nodeType === "public_field_definition" ||
      nodeType === "variable_declarator") &&
    hasJavascriptTypescriptFunctionValue(node)
  ) {
    return "function";
  }

  if (nodeType.includes("method") || nodeType.includes("constructor")) {
    return "function";
  }

  if (
    breadcrumb.length > 0 &&
    (nodeType.includes("function") ||
      nodeType === "declaration" ||
      nodeType === "function_item")
  ) {
    return "function";
  }

  if (nodeType.includes("function")) {
    return "function";
  }

  if (nodeType === "declaration" || nodeType === "macro_type_specifier") {
    return "function";
  }

  if (
    nodeType.includes("class") ||
    nodeType.includes("struct") ||
    nodeType.includes("impl") ||
    nodeType.includes("enum") ||
    nodeType.includes("union") ||
    nodeType.includes("record")
  ) {
    return "class";
  }

  if (
    nodeType.includes("interface") ||
    nodeType.includes("protocol") ||
    nodeType.includes("trait")
  ) {
    return "interface";
  }

  if (
    nodeType.includes("module") ||
    nodeType.includes("namespace") ||
    nodeType === "mod_item"
  ) {
    return "module";
  }

  if (
    nodeType.includes("alias") ||
    nodeType.includes("typedef") ||
    nodeType === "type_definition" ||
    nodeType === "type_item"
  ) {
    return "alias";
  }

  return "value";
}

function truncateInline(value: string, maxChars: number): string {
  const compact = value.replace(/\s+/g, " ").trim();

  return compact.length > maxChars
    ? `${compact.slice(0, maxChars).trimEnd()}...`
    : compact;
}

function isScriptBlockFormat(format: string): boolean {
  return format === "vue" || format === "svelte";
}

function findScriptBlocks(text: string): ScriptBlock[] {
  const blocks: ScriptBlock[] = [];
  const scriptPattern = /<script(\s[^>]*)?>([\s\S]*?)<\/script>/gi;

  for (const match of text.matchAll(scriptPattern)) {
    const fullMatch = match[0];
    const attrs = match[1] ?? "";
    const blockText = match[2] ?? "";
    const tagEndOffset = fullMatch.indexOf(">");

    if (tagEndOffset < 0) {
      continue;
    }

    const startOffset = match.index + tagEndOffset + 1;

    blocks.push({
      text: blockText,
      format: scriptBlockFormat(attrs),
      startLine: lineAtOffset(text, startOffset),
      startOffset,
    });
  }

  return blocks;
}

function scriptBlockFormat(attrs: string): ScriptBlock["format"] {
  const lang = attrs
    .match(/\blang\s*=\s*["']?([A-Za-z0-9_-]+)/i)?.[1]
    ?.toLowerCase();

  if (lang === "ts" || lang === "typescript") {
    return "typescript";
  }
  if (lang === "tsx") {
    return "tsx";
  }
  if (lang === "jsx") {
    return "jsx";
  }

  return "javascript";
}

function lineAtOffset(text: string, offset: number): number {
  let line = 1;

  for (let index = 0; index < offset; index++) {
    if (text.charCodeAt(index) === 10) {
      line++;
    }
  }

  return line;
}

function remapScriptBlockFragments(
  fileId: string,
  fragments: readonly EntityFragment[],
  startIndex: number,
  startLine: number,
  startOffset: number,
): EntityFragment[] {
  const idMap = new Map<string, string>();

  for (const [index, fragment] of fragments.entries()) {
    idMap.set(fragment.id, makeEntityId(fileId, startIndex + index));
  }

  return fragments.map((fragment) => ({
    ...fragment,
    id: idMap.get(fragment.id) ?? fragment.id,
    group: fragment.group
      ? (idMap.get(fragment.group) ?? fragment.group)
      : undefined,
    fileId,
    range: remapScriptBlockRange(fragment.range, startLine, startOffset),
  }));
}

function remapScriptBlockRange(
  range: EntityFragment["range"],
  startLine: number,
  startOffset: number,
): EntityFragment["range"] {
  if (range.kind !== "text") {
    return range;
  }

  return {
    ...range,
    startLine: startLine + range.startLine - 1,
    endLine: startLine + range.endLine - 1,
    startOffset: startOffset + range.startOffset,
    endOffset: startOffset + range.endOffset,
  };
}
