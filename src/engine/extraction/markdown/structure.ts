import type { Heading as MdastHeading, RootContent } from "mdast";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { toString } from "mdast-util-to-string";
import { gfm } from "micromark-extension-gfm";

export type MarkdownHeading = {
  level: number;
  text: string;
  lineIndex: number;
};

export type MarkdownBlock = {
  type: string;
  startIndex: number;
  endIndex: number;
};

export type MarkdownStructure = {
  headings: readonly MarkdownHeading[];
  blocks: readonly MarkdownBlock[];
};

export function parseMarkdownStructure(
  source: string,
  lineIndexOffset = 0,
): MarkdownStructure {
  const tree = fromMarkdown(source, {
    extensions: [gfm()],
    mdastExtensions: [gfmFromMarkdown()],
  });
  const blocks = tree.children.flatMap((node) =>
    markdownBlocks(node, lineIndexOffset),
  );
  const headings = tree.children.flatMap((node) =>
    node.type === "heading" ? markdownHeading(node, lineIndexOffset) : [],
  );

  return { headings, blocks };
}

function markdownHeading(
  node: MdastHeading,
  lineIndexOffset: number,
): MarkdownHeading[] {
  const startLine = node.position?.start.line;
  if (startLine === undefined) {
    return [];
  }

  return [
    {
      level: node.depth,
      text: toString(node).replace(/\s+/g, " ").trim(),
      lineIndex: lineIndexOffset + startLine - 1,
    },
  ];
}

function markdownBlocks(
  node: RootContent,
  lineIndexOffset: number,
): MarkdownBlock[] {
  const startLine = node.position?.start.line;
  const endLine = node.position?.end.line;
  if (startLine === undefined || endLine === undefined) {
    return [];
  }

  return [
    {
      type: node.type,
      startIndex: lineIndexOffset + startLine - 1,
      endIndex: lineIndexOffset + endLine - 1,
    },
    ...nestedBlockChildren(node).flatMap((child) =>
      markdownBlocks(child, lineIndexOffset),
    ),
  ];
}

function nestedBlockChildren(node: RootContent): readonly RootContent[] {
  switch (node.type) {
    case "blockquote":
    case "footnoteDefinition":
    case "list":
    case "listItem":
      return node.children;
    default:
      return [];
  }
}
